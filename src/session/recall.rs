//! Recall through the task (#117): what each agent context of a session
//! did lately and what was pushed to it, in one SQLite file beside the
//! session's snapshot.
//!
//! - The window: each action's terms (stems and anchors, never a tool's
//!   result or a file's contents), the last [`WINDOW_ACTIONS`] per context.
//! - `shown`: the passages pushed to a context in its current epoch, each
//!   with the action it was pushed at, so a passage is pushed once per
//!   stretch ([`STRETCH_ACTIONS`]) and two hooks at once push it once.
//! - `held`: the passages a hold showed a context before its command ran
//!   (#118), and those it matched but had no room for (deferred), so a
//!   passage holds once per stretch; a held one counts as pushed, a
//!   deferred one is still pushed after the command.
//! - `pending_hold` and `outcome`: each context's last hold until its next
//!   tool call, and whether that call ran the held command again
//!   (overridden) or did anything else first (heeded), per memory.
//! - The epoch: `startup` opens the first, `clear` and `compact` a new one
//!   with its own budget ([`SESSION_PUSH_CHARS`]); `resume` keeps it.
//!
//! A hook reads the window outside any lock, matches, then writes its
//! action and what it pushes in one short `BEGIN IMMEDIATE` transaction,
//! so the shown-once rule and the budget hold however many hooks run.

use super::live::RECALL;
use crate::{
    memory::{
        hold::HOLD_PER_EVENT,
        matcher::{Kind, Terms, WINDOW_ACTIONS},
        push::PUSH_PER_EVENT,
        working_set::{self, Entry, Printed},
    },
    private,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

/// The agent context of the session's own agent; a subagent's is its id.
pub(super) const MAIN: &str = "main";

/// Actions after which a passage pushed to a context may be pushed again
/// when the work matches it: "shown 400 steps ago" is not "still in mind"
/// (#117, "A stretch"). A starting value: under one compaction cycle on a
/// 200,000-token context at about 1,500 tokens a call, and over three
/// windows of [`WINDOW_ACTIONS`]. Scenario Q2 sets it.
pub(super) const STRETCH_ACTIONS: i64 = 100;

/// The characters all pushes to one context may hold in one epoch (#117):
/// bench sessions push 640 to 830 at the median and 1,630 at most, so it
/// binds only on long sessions. Scenario Q2 records pushed characters per
/// epoch and sets it.
pub(super) const SESSION_PUSH_CHARS: usize = 8_000;

/// The passage number that stands for a whole memory, as session start
/// prints a rule in full: every passage of it counts as shown. Such a row
/// holds the id as session start prints it, its first characters.
const WHOLE: i64 = -1;

/// The passage number of a push that named a memory by its id alone
/// (#293), by its full id. It stops only later such lines of that memory
/// in the stretch: the line holds no passage, so another passage's text,
/// and a hold of one, still tell the agent what the line did not. On a
/// local store the line comes only from a hook that ran out of time.
const NAMED: i64 = -2;

/// The state file's layout. An older one is upgraded in place, by running
/// [`SCHEMA`] and setting this number, so a session that moves to a new
/// binary keeps its state: that holds only while each layout change adds
/// tables (`CREATE TABLE IF NOT EXISTS`); a change that alters or drops a
/// table needs a step of its own here. A newer layout is refused, never
/// read: the file is rebuilt at the session's next `startup`. A change to
/// [`SCHEMA`] takes the next number free on `main` when it lands; a branch
/// still open takes the next after whichever lands before it, at its
/// rebase, and adds its layout's entry to the upgrade test.
const VERSION: i64 = 5;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS contexts(
    agent TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL,
    actions INTEGER NOT NULL,
    chars INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS window(
    agent TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    failed INTEGER NOT NULL,
    stems TEXT NOT NULL,
    anchors TEXT NOT NULL,
    PRIMARY KEY(agent, seq)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS spawns(
    seq INTEGER PRIMARY KEY,
    key TEXT NOT NULL,
    stems TEXT NOT NULL,
    anchors TEXT NOT NULL,
    claimed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS shown(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    passage INTEGER NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY(agent, memory, passage)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS held(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    passage INTEGER NOT NULL,
    at INTEGER NOT NULL,
    shown INTEGER NOT NULL,
    PRIMARY KEY(agent, memory, passage)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS worked(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    read INTEGER NOT NULL DEFAULT 0,
    recorded INTEGER NOT NULL DEFAULT 0,
    found INTEGER NOT NULL DEFAULT 0,
    shown INTEGER NOT NULL DEFAULT 0,
    held INTEGER NOT NULL DEFAULT 0,
    at INTEGER NOT NULL,
    epoch INTEGER NOT NULL,
    sent INTEGER NOT NULL DEFAULT 0,
    changed INTEGER NOT NULL,
    PRIMARY KEY(agent, memory)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pushed(
    memory TEXT NOT NULL,
    passage INTEGER NOT NULL,
    times INTEGER NOT NULL,
    PRIMARY KEY(memory, passage)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tag_folders(
    folder TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    common TEXT NOT NULL,
    root TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tag_pending(
    memory TEXT PRIMARY KEY,
    tag TEXT NOT NULL,
    project TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tag_stamps(
    common TEXT NOT NULL,
    path TEXT NOT NULL,
    stamp TEXT NOT NULL,
    PRIMARY KEY(common, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tag_ends(
    memory TEXT PRIMARY KEY,
    tag TEXT NOT NULL,
    head TEXT NOT NULL,
    project TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tag_flags(
    flag TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pending_hold(
    agent TEXT PRIMARY KEY,
    call TEXT NOT NULL,
    command TEXT NOT NULL,
    memories TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS outcome(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    overridden INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY(agent, memory)
) WITHOUT ROWID;
";

/// What an agent context did with a memory, as `worked` keeps it: one row
/// per context and memory (its full id), its flags only ever set. `read`:
/// the `read` tool returned it; `recorded`: `record` stored or confirmed
/// it, or `update` changed it; `found`: `search` returned it (#118);
/// `shown`: a push showed a passage of it, which `pushed` counts by
/// passage; `held`: a hold showed it (#118). `at` and `epoch` are the context's action count and epoch
/// at its last use, which #118 reads its working set by and moves to a new
/// epoch when it restores the memory: that is not a use. `sent` says the
/// session's use flush has sent the row's flags; any use sets it back to 0
/// and takes the next `changed`, so the flush marks sent only what it read.
/// The flush decides on the flags and `sent` alone, never on `at` or
/// `epoch`.
const WORKED: &str =
    "INSERT INTO worked(agent, memory, read, recorded, found, shown, held, at, epoch, sent, changed)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, (SELECT coalesce(max(changed), 0) + 1 FROM worked))
     ON CONFLICT(agent, memory) DO UPDATE SET read = max(read, excluded.read),
     recorded = max(recorded, excluded.recorded), found = max(found, excluded.found),
     shown = max(shown, excluded.shown), held = max(held, excluded.held),
     at = excluded.at, epoch = excluded.epoch, sent = 0, changed = excluded.changed";

/// How a tool call used a memory, as the post-tool hook reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum How {
    Read,
    Recorded,
    Found,
}

/// One memory the session used and has not sent, its flags joined over
/// every agent context, with the passage pushed most often (the earlier on
/// a tie), if any was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Unsent {
    pub(super) memory: String,
    pub(super) read: bool,
    pub(super) recorded: bool,
    pub(super) shown: bool,
    /// Whether a hold showed it, and whether the context last held by it
    /// ran the held command again (none until one did or did not).
    pub(super) held: bool,
    pub(super) overridden: Option<bool>,
    pub(super) passage: Option<i64>,
}

/// The `meta` key of the session's conversation origin, which a sweep
/// sends another session's use under.
const ORIGIN: &str = "origin";
/// The `meta` key set when the session's server takes no use from it:
/// nothing is sent again this session.
const NO_USE: &str = "no_use";
/// The `meta` key of the main context's action count when its epoch
/// opened: a compaction whose context has acted since then opens the next
/// epoch, and one that finds it unchanged is the same compaction, met by
/// its other handler (#118).
const OPENED: &str = "opened";
/// The `meta` key of the epoch the main context's last compaction ended,
/// which its working set restores; none after a `clear`.
const ENDED: &str = "ended";
/// The `meta` key of the agents' notes that no longer hold a command in
/// this session, as session start found them (#118).
const STOPPED: &str = "stopped";

/// How a session (re)started, as the client's session-start payload says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    Startup,
    Resume,
    Clear,
    Compact,
}

impl Source {
    /// The source a session-start payload names, as both clients name it;
    /// none for a payload without one this build knows.
    pub(super) fn of(payload: &[u8]) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct Started {
            source: String,
        }
        match serde_json::from_slice::<Started>(payload)
            .ok()?
            .source
            .as_str()
        {
            "startup" => Some(Self::Startup),
            "resume" => Some(Self::Resume),
            "clear" => Some(Self::Clear),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

/// One action as the window keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Step {
    pub(super) kind: Kind,
    pub(super) failed: bool,
    pub(super) terms: Terms,
}

/// A shell call a hold stops: its tool call's id, as the client gives it
/// (empty when it gives none), and its command's key.
#[derive(Clone, Copy, Debug)]
pub(super) struct Call<'a> {
    pub(super) id: &'a str,
    pub(super) key: &'a str,
}

/// A passage a hook would push, with the characters its line takes.
#[derive(Clone, Copy, Debug)]
pub(super) struct Offer<'a> {
    pub(super) memory: &'a str,
    pub(super) passage: i64,
    /// Whether the line names the id alone: shown once for the memory
    /// ([`NAMED`]), whichever passage matched.
    pub(super) named: bool,
    pub(super) chars: usize,
}

/// The state file of the session whose snapshot is at `snapshot`.
pub(super) fn path(snapshot: &Path) -> PathBuf {
    snapshot.with_extension(RECALL)
}

/// An open state file.
pub(super) struct State {
    conn: Connection,
}

impl State {
    /// Opens the state file at `path`, creating it private when missing.
    /// Waiting for another hook's write ends at `deadline`.
    pub(super) fn open(path: &Path, deadline: Instant) -> Result<Self> {
        let mut conn = connect(path, deadline)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        } else if version < VERSION {
            // One IMMEDIATE transaction, so of two hooks opening it at once
            // one upgrades it and the other finds it done.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if now < VERSION {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", VERSION)?;
            }
            tx.commit()?;
        } else if version != VERSION {
            bail!("{} has layout {version}, not {VERSION}", path.display());
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { conn })
    }

    /// Opens the session's own context as `source` says, with `whole` the
    /// memories session start printed in full this time, which count as
    /// shown from now. A `startup` begins anew (see [`State::reset`]);
    /// `clear` empties the window and opens an epoch; `compact` opens one
    /// once ([`compacted`]) and keeps the window, as the work goes on;
    /// `resume` keeps the epoch and what it showed.
    pub(super) fn start(
        path: &Path,
        source: Source,
        whole: &[&str],
        deadline: Instant,
    ) -> Result<Self> {
        let mut state = if source == Source::Startup {
            Self::reset(path, deadline)?
        } else {
            Self::open(path, deadline)?
        };
        let tx = state
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, actions, _) = context(&tx, MAIN)?;
        match source {
            Source::Clear => {
                tx.execute("DELETE FROM window WHERE agent = ?1", [MAIN])?;
                open_epoch(&tx, actions)?;
                tx.execute("DELETE FROM meta WHERE key = ?1", [ENDED])?;
            }
            Source::Compact => {
                compacted(&tx)?;
            }
            Source::Startup | Source::Resume => {}
        }
        mark_whole(&tx, MAIN, whole, actions)?;
        tx.commit()?;
        Ok(state)
    }

    /// Counts `whole`, memories printed in full to `agent`'s context, as
    /// shown to it from its current action: a subagent's start prints the
    /// project rules as session start does for the session's own context.
    pub(super) fn show_whole(&mut self, agent: &str, whole: &[&str]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, actions, _) = context(&tx, agent)?;
        mark_whole(&tx, agent, whole, actions)?;
        tx.commit()?;
        Ok(())
    }

    /// The state of a session starting anew: every table dropped and made
    /// again in one `IMMEDIATE` transaction, whatever layout the file had,
    /// so it works while another hook of the session holds the file open,
    /// which Windows would not let a delete do. Only a file that is not a
    /// database at all, which no hook can be using, is deleted and made
    /// again.
    fn reset(path: &Path, deadline: Instant) -> Result<Self> {
        match Self::rebuild(path, deadline) {
            Err(err) if not_a_database(&err) => {
                for file in [path.to_owned(), wal(path), shm(path)] {
                    match fs::remove_file(&file) {
                        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                            return Err(err)
                                .with_context(|| format!("Cannot remove {}", file.display()));
                        }
                        _ => {}
                    }
                }
                Self::rebuild(path, deadline)
            }
            rebuilt => rebuilt,
        }
    }

    /// Drops every table of the file at `path` and makes [`SCHEMA`] in one
    /// transaction.
    fn rebuild(path: &Path, deadline: Instant) -> Result<Self> {
        let mut conn = connect(path, deadline)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tables = tx
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for table in tables {
            tx.execute_batch(&format!("DROP TABLE \"{}\"", table.replace('"', "\"\"")))?;
        }
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.commit()?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { conn })
    }

    /// The window of `agent`'s context, oldest action first.
    pub(super) fn window(&self, agent: &str) -> Result<Vec<Step>> {
        let mut statement = self.conn.prepare(
            "SELECT kind, failed, stems, anchors FROM window WHERE agent = ?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([agent], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut steps = Vec::new();
        for row in rows {
            let (kind, failed, stems, anchors) = row?;
            let Some(kind) = kind_of(&kind) else {
                continue;
            };
            steps.push(Step {
                kind,
                failed,
                terms: Terms {
                    stems: serde_json::from_str(&stems)?,
                    anchors: serde_json::from_str(&anchors)?,
                    ..Default::default()
                },
            });
        }
        Ok(steps)
    }

    /// Adds `step` to `agent`'s window and marks as shown those of `offers`
    /// not shown in this stretch, in order, at most [`PUSH_PER_EVENT`]
    /// within `room` characters, while the epoch's budget holds; returns
    /// their positions in `offers`. An offer that names the id alone is
    /// shown once for its memory ([`NAMED`]), and not where its passage was
    /// shown. All or nothing: a hook that cannot write before its deadline
    /// pushes nothing and loses its step.
    pub(super) fn record(
        &mut self,
        agent: &str,
        step: &Step,
        offers: &[Offer],
        room: usize,
    ) -> Result<Vec<usize>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (epoch, actions, mut chars) = context(&tx, agent)?;
        let at = actions + 1;
        tx.execute(
            "INSERT INTO window(agent, seq, kind, failed, stems, anchors) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                agent,
                at,
                kind_name(step.kind),
                step.failed,
                serde_json::to_string(&step.terms.stems)?,
                serde_json::to_string(&step.terms.anchors)?,
            ],
        )?;
        let kept = i64::try_from(WINDOW_ACTIONS)?;
        tx.execute(
            "DELETE FROM window WHERE agent = ?1 AND seq <= ?2",
            params![agent, at - kept],
        )?;
        let mut shown = Vec::new();
        let mut event = 0;
        for (position, offer) in offers.iter().enumerate() {
            if shown.len() == PUSH_PER_EVENT {
                break;
            }
            if chars + offer.chars > SESSION_PUSH_CHARS || event + offer.chars > room {
                continue;
            }
            // A passage's text is shown once by its passage, which the
            // insert below checks; a line naming the id alone also checks
            // its passage here, and is shown once by NAMED.
            let (key, also) = if offer.named {
                (NAMED, offer.passage)
            } else {
                (offer.passage, WHOLE)
            };
            let seen: bool = tx
                .query_row(
                    "SELECT 1 FROM shown WHERE agent = ?1 AND memory = substr(?2, 1, length(memory)) AND passage IN (?3, ?7) AND ?4 - at <= ?5
                     UNION ALL
                     SELECT 1 FROM held WHERE agent = ?1 AND memory = ?2 AND passage = ?6 AND shown = 1 AND ?4 - at <= ?5",
                    params![agent, offer.memory, WHOLE, at, STRETCH_ACTIONS, offer.passage, also],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if seen {
                continue;
            }
            let marked = tx.execute(
                "INSERT INTO shown(agent, memory, passage, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent, memory, passage) DO UPDATE SET at = excluded.at
                 WHERE excluded.at - shown.at > ?5",
                params![agent, offer.memory, key, at, STRETCH_ACTIONS],
            )?;
            if marked == 1 {
                chars += offer.chars;
                event += offer.chars;
                shown.push(position);
                tx.execute(
                    WORKED,
                    params![agent, offer.memory, 0, 0, 0, 1, 0, at, epoch],
                )?;
                tx.execute(
                    "INSERT INTO pushed(memory, passage, times) VALUES (?1, ?2, 1)
                     ON CONFLICT(memory, passage) DO UPDATE SET times = times + 1",
                    params![offer.memory, offer.passage],
                )?;
            }
        }
        tx.execute(
            "UPDATE contexts SET actions = ?2, chars = ?3 WHERE agent = ?1",
            params![agent, at, i64::try_from(chars)?],
        )?;
        tx.commit()?;
        Ok(shown)
    }

    /// What `agent`'s context has in this stretch, so a hook skips it
    /// before reading any text: the passages pushed or printed whole, the
    /// passages a hold showed, and for [`Use::Hold`] those it deferred too.
    pub(super) fn handled(&self, agent: &str, by: Use) -> Result<Handled> {
        let (_, actions, _) = context(&self.conn, agent)?;
        let at = actions + by.ahead();
        let mut handled = Handled::default();
        let mut shown = self.conn.prepare(
            "SELECT memory, passage FROM shown WHERE agent = ?1 AND ?2 - at <= ?3
             UNION ALL
             SELECT memory, passage FROM held WHERE agent = ?1 AND ?2 - at <= ?3 AND shown >= ?4",
        )?;
        let rows = shown.query_map(
            params![agent, at, STRETCH_ACTIONS, i64::from(by == Use::Push)],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?;
        for row in rows {
            let (memory, passage) = row?;
            if passage == WHOLE {
                handled.whole.push(memory);
            } else if passage == NAMED {
                handled.named.push(memory);
            } else {
                handled.passages.insert((memory, passage));
            }
        }
        Ok(handled)
    }

    /// Holds `agent`'s command back with `offers`, the passages that hold
    /// it strongest first: the first [`HOLD_PER_EVENT`] not handled in this
    /// stretch that fit in `room` characters are held, and each other one,
    /// and each of `unread` (passing, their text not read), is deferred.
    /// Returns the held positions in `offers`; none, with nothing written,
    /// when none is held or `deadline` passes first, so the command runs
    /// and nothing is marked that the agent was not shown. A hold is kept
    /// as the held `call` until the context's next tool call settles it
    /// ([`State::settle`]).
    pub(super) fn hold(
        &mut self,
        agent: &str,
        call: Call,
        offers: &[Offer],
        unread: &[(&str, i64)],
        room: usize,
        deadline: Instant,
    ) -> Result<Vec<usize>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (epoch, at, _) = context(&tx, agent)?;
        let mark = |memory: &str, passage: i64, shown: bool| {
            tx.execute(
                "INSERT INTO held(agent, memory, passage, at, shown) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(agent, memory, passage) DO UPDATE SET at = excluded.at, shown = excluded.shown
                 WHERE excluded.at - held.at > ?6",
                params![agent, memory, passage, at, shown, STRETCH_ACTIONS],
            )
        };
        let mut held = Vec::new();
        let mut used = 0;
        let mut deferred = Vec::new();
        for (position, offer) in offers.iter().enumerate() {
            if held.len() < HOLD_PER_EVENT && used + offer.chars <= room {
                if mark(offer.memory, offer.passage, true)? == 1 {
                    used += offer.chars;
                    held.push(position);
                }
            } else {
                deferred.push((offer.memory, offer.passage));
            }
        }
        if held.is_empty() {
            return Ok(held);
        }
        for (memory, passage) in deferred.into_iter().chain(unread.iter().copied()) {
            mark(memory, passage, false)?;
        }
        for &position in &held {
            let memory = offers[position].memory;
            tx.execute(WORKED, params![agent, memory, 0, 0, 0, 0, 1, at, epoch])?;
        }
        let memories: Vec<&str> = held.iter().map(|&p| offers[p].memory).collect();
        tx.execute(
            "INSERT OR REPLACE INTO pending_hold(agent, call, command, memories) VALUES (?1, ?2, ?3, ?4)",
            params![agent, call.id, call.key, serde_json::to_string(&memories)?],
        )?;
        if Instant::now() >= deadline {
            return Ok(Vec::new());
        }
        tx.commit()?;
        Ok(held)
    }
}

impl State {
    /// The main context's working set as its compaction left it (#118):
    /// the epoch that ended ([`compacted`], so either compact handler
    /// opens the next one) and what the context did with each memory in
    /// it, newest first, with the passages pushed of it, most pushed
    /// first. None after a `clear` with no action since. Rows a restore
    /// moved to the new epoch are read again, so two compactions with no
    /// action between them restore the same set.
    pub(super) fn working_set(&mut self) -> Result<Option<(i64, Vec<Entry>)>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(ended) = compacted(&tx)? else {
            tx.commit()?;
            return Ok(None);
        };
        let entries = entries(&tx, ended)?;
        tx.commit()?;
        Ok(Some((ended, entries)))
    }

    /// The main context's working set as it stands, before a compaction
    /// ends its epoch: what PreCompact asks the summary to keep.
    pub(super) fn current_set(&self) -> Result<Vec<Entry>> {
        let (epoch, _, _) = context(&self.conn, MAIN)?;
        entries(&self.conn, epoch)
    }

    /// Moves what the working set `printed` to the main context's current
    /// epoch, which is not a use, and counts it as shown from now: whole,
    /// or by the passages it showed. A pointer is not shown.
    pub(super) fn restored(&mut self, printed: &[Printed]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (epoch, actions, _) = context(&tx, MAIN)?;
        for one in printed {
            tx.execute(
                "UPDATE worked SET epoch = ?3 WHERE agent = ?1 AND memory = ?2",
                params![MAIN, one.from, epoch],
            )?;
            if one.whole {
                mark_whole(&tx, MAIN, &[one.id.as_str()], actions)?;
            }
            for passage in &one.passages {
                tx.execute(
                    "INSERT OR REPLACE INTO shown(agent, memory, passage, at) VALUES (?1, ?2, ?3, ?4)",
                    params![MAIN, one.id, passage, actions],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Which hook asks what a context has had: a push skips what was pushed
/// or held, a hold skips those and what it deferred too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Use {
    Push,
    Hold,
}

impl Use {
    /// How far ahead of the context's last action the hook stands: a push
    /// comes after its action, which then counts, a hold before it.
    fn ahead(self) -> i64 {
        match self {
            Self::Push => 1,
            Self::Hold => 0,
        }
    }
}

/// The passages a context had in this stretch ([`State::handled`]).
#[derive(Debug, Default)]
pub(super) struct Handled {
    passages: std::collections::HashSet<(String, i64)>,
    /// The ids, as session start prints them, of memories printed whole.
    whole: Vec<String>,
    /// The full ids of memories a push named by the id alone ([`NAMED`]).
    named: Vec<String>,
}

impl Handled {
    /// Whether the passage `passage` of memory `id` is among them.
    pub(super) fn contains(&self, id: &str, passage: i64) -> bool {
        self.whole
            .iter()
            .any(|short| id.starts_with(short.as_str()))
            || self.passages.contains(&(id.to_owned(), passage))
    }

    /// Whether memory `id` was named by its id alone ([`NAMED`]).
    pub(super) fn named(&self, id: &str) -> bool {
        self.named.iter().any(|named| named == id)
    }
}

impl State {
    /// Counts `memories` (full ids) as used by `agent`'s context, as `how`
    /// says, at its current action.
    pub(super) fn used(&mut self, agent: &str, memories: &[String], how: How) -> Result<()> {
        if memories.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (epoch, actions, _) = context(&tx, agent)?;
        for memory in memories {
            tx.execute(
                WORKED,
                params![
                    agent,
                    memory,
                    how == How::Read,
                    how == How::Recorded,
                    how == How::Found,
                    0,
                    0,
                    actions,
                    epoch
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Settles `agent`'s last hold at its next tool call `call`, whose shell
    /// command's key is `command` (none for any other tool); the held call
    /// itself, should a client report it, settles nothing. The agent
    /// overrode the hold when that call runs the held command again, and
    /// heeded it when it did anything else first, such as an edit or a
    /// read, even if the same command follows (#118). Each memory the hold
    /// showed keeps that outcome, the latest per context, and is sent
    /// again with it.
    pub(super) fn settle(&mut self, agent: &str, call: &str, command: Option<&str>) -> Result<()> {
        // Most calls follow no hold: a read finds that without the write
        // lock, which another hook of the session may hold. The hold is read
        // again under the lock, since another hook may settle it between.
        let waiting: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM pending_hold WHERE agent = ?1",
                [agent],
                |r| r.get(0),
            )
            .optional()?;
        if waiting.is_none() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: Option<(String, String, String)> = tx
            .query_row(
                "SELECT call, command, memories FROM pending_hold WHERE agent = ?1",
                [agent],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((held_call, held, memories)) = pending else {
            return Ok(());
        };
        if !call.is_empty() && call == held_call {
            return Ok(());
        }
        let overridden = command == Some(held.as_str());
        for memory in serde_json::from_str::<Vec<String>>(&memories)? {
            tx.execute(
                "INSERT OR REPLACE INTO outcome(agent, memory, overridden, seq)
                 VALUES (?1, ?2, ?3, (SELECT coalesce(max(seq), 0) + 1 FROM outcome))",
                params![agent, memory, overridden],
            )?;
            tx.execute(
                "UPDATE worked SET sent = 0, changed = (SELECT coalesce(max(changed), 0) + 1 FROM worked)
                 WHERE agent = ?1 AND memory = ?2",
                params![agent, memory],
            )?;
        }
        tx.execute("DELETE FROM pending_hold WHERE agent = ?1", [agent])?;
        tx.commit()?;
        Ok(())
    }

    /// Keeps `stopped`, the agents' notes that no longer hold a command in
    /// this session, as session start found them in the store (#118).
    pub(super) fn set_stopped(&mut self, stopped: &[String]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![STOPPED, serde_json::to_string(stopped)?],
        )?;
        Ok(())
    }

    /// The agents' notes that no longer hold a command in this session.
    pub(super) fn stopped(&self) -> Result<Vec<String>> {
        let found: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [STOPPED], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(match found {
            Some(text) => serde_json::from_str(&text)?,
            None => Vec::new(),
        })
    }

    /// Keeps the session's conversation origin, which its use is sent under.
    pub(super) fn set_origin(&mut self, origin: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![ORIGIN, origin],
        )?;
        Ok(())
    }

    /// The session's conversation origin, when a hook kept it.
    pub(super) fn origin(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [ORIGIN], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// The memories used and not sent, and the `changed` mark they were
    /// read at, which [`State::sent`] takes; none when the session's server
    /// takes no use from it.
    pub(super) fn unsent(&self) -> Result<(Vec<Unsent>, i64)> {
        let refused: bool = self
            .conn
            .query_row("SELECT 1 FROM meta WHERE key = ?1", [NO_USE], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        let mark: i64 =
            self.conn
                .query_row("SELECT coalesce(max(changed), 0) FROM worked", [], |r| {
                    r.get(0)
                })?;
        if refused {
            return Ok((Vec::new(), mark));
        }
        let mut statement = self.conn.prepare(
            "SELECT memory, max(read), max(recorded), max(shown), max(held),
             (SELECT overridden FROM outcome WHERE outcome.memory = worked.memory ORDER BY seq DESC LIMIT 1),
             (SELECT passage FROM pushed WHERE pushed.memory = worked.memory ORDER BY times DESC, passage LIMIT 1)
             FROM worked WHERE memory IN (SELECT memory FROM worked WHERE sent = 0 AND changed <= ?1)
             AND (read OR recorded OR shown OR held)
             GROUP BY memory ORDER BY memory",
        )?;
        let rows = statement.query_map([mark], |r| {
            Ok(Unsent {
                memory: r.get(0)?,
                read: r.get(1)?,
                recorded: r.get(2)?,
                shown: r.get(3)?,
                held: r.get(4)?,
                overridden: r.get(5)?,
                passage: r.get(6)?,
            })
        })?;
        Ok((rows.collect::<rusqlite::Result<_>>()?, mark))
    }

    /// Marks sent what [`State::unsent`] read at `mark`: a use since then
    /// stays unsent.
    pub(super) fn sent(&mut self, mark: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE worked SET sent = 1 WHERE sent = 0 AND changed <= ?1",
            [mark],
        )?;
        Ok(())
    }

    /// Notes that the session's server takes no use from it (a read-only
    /// token, or a server from before the route): nothing more is sent.
    pub(super) fn refuse_use(&mut self) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, '1')",
            [NO_USE],
        )?;
        Ok(())
    }
}

/// What the tag check (#121) keeps of a session's checkouts: per working
/// folder, its project and git folder (`tag_folders`, looked up once); the
/// tag conditions pending for each project, as the store last listed them
/// (`tag_pending`); the stamps of each git folder's refs at the last
/// lookup (`tag_stamps`); ends found but not yet written to the store
/// (`tag_ends`), which count as ended here until they are; and what the
/// session's server said of the check (`tag_flags`).
impl State {
    /// The project and git folder kept for `folder`; `common` is empty for
    /// a folder outside git.
    pub(super) fn tag_folder(&self, folder: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project, common FROM tag_folders WHERE folder = ?1",
                [folder],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Keeps `folder`'s project, git folder and the top folder of its
    /// checkout (empty outside one).
    pub(super) fn keep_tag_folder(
        &mut self,
        folder: &str,
        project: &str,
        common: &str,
        root: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO tag_folders(folder, project, common, root) VALUES (?1, ?2, ?3, ?4)",
            params![folder, project, common, root],
        )?;
        Ok(())
    }

    /// The project and git folder of the checkout whose top folder is
    /// `root`, from any folder of it kept before.
    pub(super) fn checkout_at(&self, root: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project, common FROM tag_folders WHERE root = ?1 AND root != '' LIMIT 1",
                [root],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Replaces the tag conditions pending for `project` with `pending`
    /// (memory, tag).
    pub(super) fn keep_pending(
        &mut self,
        project: &str,
        pending: &[(String, String)],
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM tag_pending WHERE project = ?1", [project])?;
        for (memory, tag) in pending {
            tx.execute(
                "INSERT OR REPLACE INTO tag_pending(memory, tag, project) VALUES (?1, ?2, ?3)",
                params![memory, tag, project],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The tag conditions pending for `project` with no end found yet
    /// (memory, tag).
    pub(super) fn pending(&self, project: &str) -> Result<Vec<(String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT memory, tag FROM tag_pending WHERE project = ?1
             AND memory NOT IN (SELECT memory FROM tag_ends) ORDER BY memory",
        )?;
        let rows = statement.query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the stamps of `common`'s refs are `stamps`, as at the last
    /// lookup.
    pub(super) fn same_stamps(&self, common: &str, stamps: &[(String, String)]) -> Result<bool> {
        let mut statement = self
            .conn
            .prepare("SELECT path, stamp FROM tag_stamps WHERE common = ?1 ORDER BY path")?;
        let kept: Vec<(String, String)> = statement
            .query_map([common], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut given = stamps.to_vec();
        given.sort();
        Ok(kept == given)
    }

    /// Keeps `stamps` as `common`'s refs at this lookup.
    pub(super) fn keep_stamps(&mut self, common: &str, stamps: &[(String, String)]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM tag_stamps WHERE common = ?1", [common])?;
        for (path, stamp) in stamps {
            tx.execute(
                "INSERT INTO tag_stamps(common, path, stamp) VALUES (?1, ?2, ?3)",
                params![common, path, stamp],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Keeps ends found in a checkout of `project` (memory, tag, commit)
    /// until the store has them.
    pub(super) fn keep_ends(
        &mut self,
        project: &str,
        ends: &[(String, String, String)],
    ) -> Result<()> {
        for (memory, tag, head) in ends {
            self.conn.execute(
                "INSERT OR IGNORE INTO tag_ends(memory, tag, head, project) VALUES (?1, ?2, ?3, ?4)",
                params![memory, tag, head, project],
            )?;
        }
        Ok(())
    }

    /// Whether any tag is waited for, in any project.
    pub(super) fn waits_for_tags(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT EXISTS(SELECT 1 FROM tag_pending)", [], |r| r.get(0))?)
    }

    /// The ends not yet in the store (project, memory, tag, commit).
    pub(super) fn unwritten_ends(&self) -> Result<Vec<(String, String, String, String)>> {
        let mut statement = self
            .conn
            .prepare("SELECT project, memory, tag, head FROM tag_ends ORDER BY project, memory")?;
        let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forgets the ends of `project` the store now has, and their pending
    /// conditions.
    pub(super) fn written_ends(&mut self, project: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM tag_pending WHERE memory IN (SELECT memory FROM tag_ends WHERE project = ?1)",
            [project],
        )?;
        tx.execute("DELETE FROM tag_ends WHERE project = ?1", [project])?;
        tx.commit()?;
        Ok(())
    }

    /// Whether the session's server said `flag` of the tag check.
    pub(super) fn tag_flag(&self, flag: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM tag_flags WHERE flag = ?1",
                [flag],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Keeps `flag` for the rest of the session.
    pub(super) fn keep_tag_flag(&mut self, flag: &str) -> Result<()> {
        self.conn
            .execute("INSERT OR IGNORE INTO tag_flags(flag) VALUES (?1)", [flag])?;
        Ok(())
    }

    /// The memories ended here by a tag whose end the store doesn't have
    /// yet: no push names them.
    pub(super) fn ended_here(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT memory FROM tag_ends")?;
        let rows = statement.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

impl State {
    /// Keeps a subagent's task, the terms of the words its parent gave it,
    /// for the subagent's start to claim by `key`.
    pub(super) fn spawn(&mut self, key: &str, terms: &Terms) -> Result<()> {
        self.conn.execute(
            "INSERT INTO spawns(key, stems, anchors) VALUES (?1, ?2, ?3)",
            params![
                key,
                serde_json::to_string(&terms.stems)?,
                serde_json::to_string(&terms.anchors)?
            ],
        )?;
        Ok(())
    }

    /// The oldest task kept by `key` no subagent has claimed, now claimed:
    /// of two subagents starting at once, each gets its own.
    pub(super) fn claim(&mut self, key: &str) -> Result<Option<Terms>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = tx
            .query_row(
                "SELECT seq, stems, anchors FROM spawns WHERE key = ?1 AND claimed = 0 ORDER BY seq LIMIT 1",
                [key],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?;
        let Some((seq, stems, anchors)) = found else {
            return Ok(None);
        };
        tx.execute("UPDATE spawns SET claimed = 1 WHERE seq = ?1", [seq])?;
        tx.commit()?;
        Ok(Some(Terms {
            stems: serde_json::from_str(&stems)?,
            anchors: serde_json::from_str(&anchors)?,
            ..Default::default()
        }))
    }
}

/// A connection to the state file at `path`, made private when missing,
/// that waits for another hook's write until `deadline`.
fn connect(path: &Path, deadline: Instant) -> Result<Connection> {
    let dir = path.parent().context("a state file has a folder")?;
    private::create_dir(dir)?;
    drop(private::create_file(
        fs::OpenOptions::new().write(true).create(true),
        path,
    )?);
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
    Ok(conn)
}

/// Whether `err` says the file is not a SQLite database, or a damaged one.
fn not_a_database(err: &anyhow::Error) -> bool {
    err.downcast_ref::<rusqlite::Error>()
        .and_then(rusqlite::Error::sqlite_error_code)
        .is_some_and(|code| {
            matches!(
                code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            )
        })
}

/// `agent`'s context row, made when missing: its epoch, the actions it has
/// taken and the characters pushed to it in this epoch.
fn context(conn: &Connection, agent: &str) -> Result<(i64, i64, usize)> {
    conn.execute(
        "INSERT OR IGNORE INTO contexts(agent, epoch, actions, chars) VALUES (?1, 1, 0, 0)",
        [agent],
    )?;
    let (epoch, actions, chars): (i64, i64, i64) = conn.query_row(
        "SELECT epoch, actions, chars FROM contexts WHERE agent = ?1",
        [agent],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok((epoch, actions, usize::try_from(chars)?))
}

/// What the main context did with each memory in epochs from `epoch` on,
/// newest first, with the passages pushed of it, most pushed first.
fn entries(conn: &Connection, epoch: i64) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut rows = conn.prepare(
        "SELECT memory, read, recorded, held, shown FROM worked
             WHERE agent = ?1 AND epoch >= ?2 ORDER BY at DESC, changed DESC",
    )?;
    let mut pushed =
        conn.prepare("SELECT passage FROM pushed WHERE memory = ?1 ORDER BY times DESC, passage")?;
    let found = rows.query_map(params![MAIN, epoch], |r| {
        Ok((
            r.get::<_, String>(0)?,
            [r.get::<_, bool>(1)?, r.get(2)?, r.get(3)?, r.get(4)?],
        ))
    })?;
    for row in found {
        let (memory, [read, recorded, held, shown]) = row?;
        let how = if read {
            working_set::Use::Read
        } else if recorded {
            working_set::Use::Recorded
        } else if held {
            working_set::Use::Held
        } else if shown {
            working_set::Use::Pushed
        } else {
            working_set::Use::Found
        };
        let passages = pushed
            .query_map([&memory], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        entries.push(Entry {
            id: memory,
            how,
            passages,
        });
    }
    Ok(entries)
}

/// Opens the main context's next epoch, at its action count `actions`: what
/// it was shown or held is forgotten, and its push budget starts again.
fn open_epoch(conn: &Connection, actions: i64) -> Result<()> {
    conn.execute("DELETE FROM shown WHERE agent = ?1", [MAIN])?;
    conn.execute("DELETE FROM held WHERE agent = ?1", [MAIN])?;
    conn.execute(
        "UPDATE contexts SET epoch = epoch + 1, chars = 0 WHERE agent = ?1",
        [MAIN],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![OPENED, actions.to_string()],
    )?;
    Ok(())
}

/// The epoch the main context's compaction ended, its next epoch opened
/// once however its two session-start handlers run (#118): both clients
/// run session start and the working set at once on `compact`, and the
/// first to get here opens the epoch while the second finds it opened at
/// the same action and only reads which epoch ended. A compaction with no
/// action since the last one is the same stretch too, and restores its
/// set again. None when the epoch was opened by a `clear` with no action
/// since, which restores nothing. Run in an `IMMEDIATE` transaction.
fn compacted(conn: &Connection) -> Result<Option<i64>> {
    let (epoch, actions, _) = context(conn, MAIN)?;
    let opened: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [OPENED], |r| {
            r.get(0)
        })
        .optional()?;
    if opened.and_then(|o| o.parse::<i64>().ok()) == Some(actions) {
        let ended: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [ENDED], |r| {
                r.get(0)
            })
            .optional()?;
        return Ok(ended.and_then(|e| e.parse().ok()));
    }
    open_epoch(conn, actions)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![ENDED, epoch.to_string()],
    )?;
    Ok(Some(epoch))
}

/// Marks each of `whole`, by the id as its print shows it, as shown in full
/// to `agent` at action `at`: [`State::record`] then skips every passage of
/// it for a stretch.
fn mark_whole(conn: &Connection, agent: &str, whole: &[&str], at: i64) -> Result<()> {
    for memory in whole {
        conn.execute(
            "INSERT OR REPLACE INTO shown(agent, memory, passage, at) VALUES (?1, ?2, ?3, ?4)",
            params![agent, memory, WHOLE, at],
        )?;
    }
    Ok(())
}

/// SQLite's write-ahead log beside the state file at `path`.
fn wal(path: &Path) -> PathBuf {
    path.with_extension(format!("{RECALL}-wal"))
}

/// SQLite's shared-memory index beside the state file at `path`.
fn shm(path: &Path) -> PathBuf {
    path.with_extension(format!("{RECALL}-shm"))
}

/// The name a step's kind is stored by.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Prompt => "prompt",
        Kind::Task => "task",
        Kind::Plan => "plan",
        Kind::Command => "command",
        Kind::File => "file",
        Kind::Message => "message",
        Kind::Search => "search",
    }
}

/// The kind stored as `name`; none for a name this build does not know.
fn kind_of(name: &str) -> Option<Kind> {
    [
        Kind::Prompt,
        Kind::Task,
        Kind::Plan,
        Kind::Command,
        Kind::File,
        Kind::Message,
        Kind::Search,
    ]
    .into_iter()
    .find(|kind| kind_name(*kind) == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeSet,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    /// Room for a test's hook on a loaded machine.
    const ROOM: Duration = Duration::from_secs(30);
    /// A push's room in these tests, more than any offers here take.
    const ROOM_CHARS: usize = SESSION_PUSH_CHARS;

    fn soon() -> Instant {
        Instant::now() + ROOM
    }

    fn step(kind: Kind, stems: &[&str], anchors: &[&str]) -> Step {
        Step {
            kind,
            failed: false,
            terms: Terms {
                stems: stems.iter().map(|s| (*s).to_owned()).collect(),
                anchors: anchors.iter().map(|s| (*s).to_owned()).collect(),
                ..Default::default()
            },
        }
    }

    fn command() -> Step {
        step(Kind::Command, &["cargo", "publish"], &["cargo publish"])
    }

    fn offer(memory: &str, passage: i64) -> Offer<'_> {
        Offer {
            memory,
            passage,
            named: false,
            chars: 100,
        }
    }

    fn file(dir: &tempfile::TempDir) -> PathBuf {
        path(&dir.path().join("sessions/claude-s1-key.json"))
    }

    #[test]
    fn a_passage_is_shown_once_per_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let offers = [offer("e0000001", 0), offer("e0000001", 1)];
        assert_eq!(
            state.record(MAIN, &command(), &offers, ROOM_CHARS).unwrap(),
            [0, 1]
        );
        assert_eq!(
            state.record(MAIN, &command(), &offers, ROOM_CHARS).unwrap(),
            [] as [usize; 0]
        );
        // A subagent hasn't seen what its parent saw.
        assert_eq!(
            state
                .record("agent-1", &command(), &offers, ROOM_CHARS)
                .unwrap(),
            [0, 1]
        );
        assert_eq!(path.extension().unwrap(), RECALL);
        for private in [&path, path.parent().unwrap()] {
            assert!(
                private::is_private(private).unwrap(),
                "{}",
                private.display()
            );
        }
    }

    #[test]
    fn two_concurrent_hooks_show_a_passage_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        State::start(&path, Source::Startup, &[], soon()).unwrap();
        for hooks in [2, 4] {
            for round in 0..10 {
                let memory = format!("e000000{hooks}-{round}");
                let barrier = Arc::new(Barrier::new(hooks));
                let shown: usize = (0..hooks)
                    .map(|_| {
                        let (path, memory, barrier) =
                            (path.clone(), memory.clone(), barrier.clone());
                        thread::spawn(move || {
                            let mut state = State::open(&path, soon()).unwrap();
                            barrier.wait();
                            state
                                .record(MAIN, &command(), &[offer(&memory, 0)], ROOM_CHARS)
                                .unwrap()
                                .len()
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|hook| hook.join().unwrap())
                    .sum();
                assert_eq!(shown, 1, "{hooks} hooks, round {round}");
            }
        }
        // Every hook's step joined the window.
        let state = State::open(&path, soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap().len(), WINDOW_ACTIONS);
    }

    #[test]
    fn concurrent_hooks_hold_the_epoch_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        State::start(&path, Source::Startup, &[], soon()).unwrap();
        for hooks in [2, 4] {
            // Each passage takes over half the budget: one fits per epoch.
            let chars = SESSION_PUSH_CHARS / 2 + 1;
            for round in 0..10 {
                State::start(&path, Source::Clear, &[], soon()).unwrap();
                let barrier = Arc::new(Barrier::new(hooks));
                let shown: usize = (0..hooks)
                    .map(|hook| {
                        let (path, barrier) = (path.clone(), barrier.clone());
                        thread::spawn(move || {
                            let mut state = State::open(&path, soon()).unwrap();
                            let memory = format!("e000000{hook}");
                            let offers = [Offer {
                                memory: &memory,
                                passage: 0,
                                named: false,
                                chars,
                            }];
                            barrier.wait();
                            state
                                .record(MAIN, &command(), &offers, ROOM_CHARS)
                                .unwrap()
                                .len()
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|hook| hook.join().unwrap())
                    .sum();
                assert_eq!(shown, 1, "{hooks} hooks, round {round}");
            }
        }
    }

    #[test]
    fn the_budget_holds_per_epoch_and_a_new_one_restarts_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let big = |memory| Offer {
            memory,
            passage: 0,
            named: false,
            chars: SESSION_PUSH_CHARS - 100,
        };
        // A passage past what is left is skipped; a smaller one after it fits.
        let offers = [big("e0000001"), big("e0000002"), offer("e0000003", 0)];
        assert_eq!(
            state.record(MAIN, &command(), &offers, ROOM_CHARS).unwrap(),
            [0, 2]
        );
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000004", 0)], ROOM_CHARS)
                .unwrap(),
            [] as [usize; 0]
        );
        // Another context has its own budget.
        assert_eq!(
            state
                .record("agent-1", &command(), &[big("e0000002")], ROOM_CHARS)
                .unwrap(),
            [0]
        );
        for source in [Source::Clear, Source::Compact] {
            let mut state = State::start(&path, source, &[], soon()).unwrap();
            assert_eq!(
                state
                    .record(MAIN, &command(), &[big("e0000005")], ROOM_CHARS)
                    .unwrap(),
                [0],
                "{source:?}"
            );
        }
        // resume keeps the epoch and what is left of its budget: 100, to
        // the character.
        let mut state = State::start(&path, Source::Resume, &[], soon()).unwrap();
        let over = Offer {
            memory: "e0000006",
            passage: 0,
            named: false,
            chars: 101,
        };
        assert_eq!(
            state.record(MAIN, &command(), &[over], ROOM_CHARS).unwrap(),
            [] as [usize; 0]
        );
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000006", 0)], ROOM_CHARS)
                .unwrap(),
            [0]
        );
    }

    #[test]
    fn a_push_holds_at_most_push_per_event_within_its_room() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let ids = ["e0000001", "e0000002", "e0000003", "e0000004", "e0000005"];
        let offers: Vec<Offer> = ids.iter().map(|id| offer(id, 0)).collect();
        assert_eq!(
            state.record(MAIN, &command(), &offers, ROOM_CHARS).unwrap(),
            (0..PUSH_PER_EVENT).collect::<Vec<_>>()
        );
        // The rest are not marked: the next action may push them.
        assert_eq!(
            state.record(MAIN, &command(), &offers, ROOM_CHARS).unwrap(),
            [3, 4]
        );
        // A line past the push's room is skipped and a shorter one after it
        // fits.
        let lines = [
            offer("e0000006", 0),
            Offer {
                memory: "e0000007",
                passage: 0,
                named: false,
                chars: 151,
            },
            Offer {
                memory: "e0000008",
                passage: 0,
                named: false,
                chars: 150,
            },
        ];
        assert_eq!(state.record(MAIN, &command(), &lines, 250).unwrap(), [0, 2]);
    }

    #[test]
    fn a_passage_shown_more_than_stretch_actions_ago_is_shown_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &["e0000009"], soon()).unwrap();
        let rule = [offer("e0000001", 2)];
        let printed = [offer("e0000009", 0)];
        assert_eq!(
            state.record(MAIN, &command(), &rule, ROOM_CHARS).unwrap(),
            [0]
        );
        // Shown at action 1; actions 2 to 1 + STRETCH_ACTIONS are within it.
        for _ in 1..STRETCH_ACTIONS {
            state.record(MAIN, &command(), &[], ROOM_CHARS).unwrap();
        }
        assert_eq!(
            state.record(MAIN, &command(), &rule, ROOM_CHARS).unwrap(),
            [] as [usize; 0]
        );
        assert_eq!(
            state.record(MAIN, &command(), &rule, ROOM_CHARS).unwrap(),
            [0]
        );
        assert_eq!(
            state.record(MAIN, &command(), &rule, ROOM_CHARS).unwrap(),
            [] as [usize; 0]
        );
        // A memory session start printed whole, at action 0, likewise.
        assert_eq!(
            state
                .record(MAIN, &command(), &printed, ROOM_CHARS)
                .unwrap(),
            [0]
        );
        // What a resume prints whole counts as shown from then.
        let mut state = State::start(&path, Source::Resume, &["e0000008"], soon()).unwrap();
        let reprinted = [offer("e0000008", 1)];
        assert_eq!(
            state
                .record(MAIN, &command(), &reprinted, ROOM_CHARS)
                .unwrap(),
            [] as [usize; 0]
        );
    }

    // A line naming the id alone holds no passage (#293): it is shown once
    // for its memory, and stops later such lines alone. A passage's text,
    // and a hold, still come after it.
    #[test]
    fn a_line_naming_the_id_alone_stops_only_later_such_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &["e0000008"], soon()).unwrap();
        let id = "e0000007-0000-4000-8000-000000000007";
        let pointer = |memory, passage| Offer {
            named: true,
            ..offer(memory, passage)
        };
        let mut record = |agent: &str, offer: Offer| {
            state
                .record(agent, &command(), &[offer], ROOM_CHARS)
                .unwrap()
                .len()
        };
        assert_eq!(record(MAIN, offer(id, 1)), 1);
        assert_eq!(record(MAIN, pointer(id, 1)), 0, "its passage was shown");
        assert_eq!(record(MAIN, pointer(id, 2)), 1);
        assert_eq!(record(MAIN, pointer(id, 3)), 0, "named already");
        assert_eq!(record("a1", pointer(id, 3)), 1, "to that context alone");
        for passage in [2, 3] {
            assert_eq!(record(MAIN, offer(id, passage)), 1, "text of {passage}");
        }
        let printed = "e0000008-0000-4000-8000-000000000008";
        assert_eq!(record(MAIN, pointer(printed, 1)), 0, "printed whole");
        let handled = state.handled(MAIN, Use::Push).unwrap();
        assert!(handled.named(id) && !handled.contains(id, 4));
        assert!(!state.handled(MAIN, Use::Hold).unwrap().contains(id, 4));
        let call = Call {
            id: "t-1",
            key: "k1",
        };
        assert_eq!(
            state
                .hold(MAIN, call, &[offer(id, 4)], &[], ROOM_CHARS, soon())
                .unwrap(),
            [0],
            "a hold still holds"
        );
    }

    #[test]
    fn rules_printed_whole_to_a_subagent_count_as_shown_to_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        state.show_whole("a1", &["e0000009"]).unwrap();
        let rule = [offer("e0000009-0000-4000-8000-000000000009", 0)];
        assert_eq!(
            state.record("a1", &command(), &rule, ROOM_CHARS).unwrap(),
            [] as [usize; 0],
            "printed to the subagent"
        );
        assert_eq!(
            state.record(MAIN, &command(), &rule, ROOM_CHARS).unwrap(),
            [0],
            "its parent was never shown it"
        );
        assert_eq!(
            state.record("a2", &command(), &rule, ROOM_CHARS).unwrap(),
            [0],
            "nor another subagent"
        );
    }

    #[test]
    fn session_start_opens_epochs_as_the_acceptance_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &["e0000009"], soon()).unwrap();
        let both = [offer("e0000001", 0), offer("e0000002", 0)];
        assert_eq!(
            state.record(MAIN, &command(), &both, ROOM_CHARS).unwrap(),
            [0, 1]
        );
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000009", 3)], ROOM_CHARS)
                .unwrap(),
            [] as [usize; 0],
            "printed whole at session start"
        );
        // Session start prints an id's first characters; a passage of that
        // memory, by its full id, counts as shown.
        let full = offer("e0000009-0000-4000-8000-000000000009", 1);
        assert_eq!(
            state.record(MAIN, &command(), &[full], ROOM_CHARS).unwrap(),
            [] as [usize; 0]
        );
        state
            .record("agent-1", &command(), &both, ROOM_CHARS)
            .unwrap();

        // resume: nothing shown before is pushed again.
        drop(state);
        let mut state = State::start(&path, Source::Resume, &[], soon()).unwrap();
        assert_eq!(
            state.record(MAIN, &command(), &both, ROOM_CHARS).unwrap(),
            [] as [usize; 0]
        );
        assert_eq!(state.window(MAIN).unwrap().len(), 4);

        // compact: the window goes on; only what session start restored
        // counts as shown.
        drop(state);
        let mut state = State::start(&path, Source::Compact, &["e0000001"], soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap().len(), 4);
        assert_eq!(
            state.record(MAIN, &command(), &both, ROOM_CHARS).unwrap(),
            [1]
        );
        // A subagent's context is its own.
        assert_eq!(
            state
                .record("agent-1", &command(), &both, ROOM_CHARS)
                .unwrap(),
            [] as [usize; 0]
        );

        // clear: an empty window, and every passage can be pushed again.
        drop(state);
        let mut state = State::start(&path, Source::Clear, &[], soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap(), []);
        assert_eq!(
            state.record(MAIN, &command(), &both, ROOM_CHARS).unwrap(),
            [0, 1]
        );

        // A new session, or a startup that reuses an id: no rows.
        drop(state);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap(), []);
        assert_eq!(state.window("agent-1").unwrap(), []);
        assert_eq!(
            state.record(MAIN, &command(), &both, ROOM_CHARS).unwrap(),
            [0, 1]
        );
        let other = path.with_file_name("claude-s2-key.recall");
        drop(state);
        let mut state = State::start(&other, Source::Startup, &[], soon()).unwrap();
        assert_eq!(
            state.record(MAIN, &command(), &both, ROOM_CHARS).unwrap(),
            [0, 1]
        );
    }

    /// The epoch of the main context in the file at `path`.
    fn epoch_of(path: &Path) -> i64 {
        let state = State::open(path, soon()).unwrap();
        context(&state.conn, MAIN).unwrap().0
    }

    /// A session at `path` that pushed `memory` once, at its first action.
    fn worked_on(path: &Path, memory: &str) {
        let mut state = State::start(path, Source::Startup, &[], soon()).unwrap();
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer(memory, 0)], ROOM_CHARS)
                .unwrap(),
            [0]
        );
    }

    // Both compact handlers run at once; whichever comes first opens the
    // epoch, and the working set reads the epoch that ended, in either
    // order and on two threads at once (#118).
    #[test]
    fn a_compaction_opens_one_epoch_however_its_handlers_run() {
        let ids = |set: Option<(i64, Vec<Entry>)>| {
            set.map(|(ended, entries)| {
                (
                    ended,
                    entries
                        .into_iter()
                        .map(|e| (e.id, e.how))
                        .collect::<Vec<_>>(),
                )
            })
        };
        let expected = Some((1, vec![("e0000001".to_owned(), working_set::Use::Pushed)]));
        for order in ["start first", "set first", "at once"] {
            let dir = tempfile::tempdir().unwrap();
            let path = file(&dir);
            worked_on(&path, "e0000001");
            let set = match order {
                "start first" => {
                    State::start(&path, Source::Compact, &[], soon()).unwrap();
                    State::open(&path, soon()).unwrap().working_set().unwrap()
                }
                "set first" => {
                    let set = State::open(&path, soon()).unwrap().working_set().unwrap();
                    State::start(&path, Source::Compact, &[], soon()).unwrap();
                    set
                }
                _ => {
                    let barrier = Arc::new(Barrier::new(2));
                    let (b, p) = (barrier.clone(), path.clone());
                    let start = thread::spawn(move || {
                        b.wait();
                        State::start(&p, Source::Compact, &[], soon()).unwrap();
                    });
                    barrier.wait();
                    let set = State::open(&path, soon()).unwrap().working_set().unwrap();
                    start.join().unwrap();
                    set
                }
            };
            assert_eq!(ids(set), expected, "{order}");
            assert_eq!(epoch_of(&path), 2, "{order}: one epoch opened");
            // The new epoch pushes the passage again: its earlier copy is gone.
            let mut state = State::open(&path, soon()).unwrap();
            assert_eq!(
                state
                    .record(MAIN, &command(), &[offer("e0000001", 0)], ROOM_CHARS)
                    .unwrap(),
                [0],
                "{order}"
            );
        }
    }

    // A compaction with no action since the last one is the same stretch:
    // it opens no epoch and its working set is the same. After a clear with
    // no action since, a compaction restores nothing.
    #[test]
    fn a_compaction_without_an_action_since_restores_the_same_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        worked_on(&path, "e0000001");
        let mut state = State::start(&path, Source::Compact, &[], soon()).unwrap();
        let first = state.working_set().unwrap().unwrap();
        state
            .restored(&[Printed {
                from: "e0000001".to_owned(),
                id: "e0000001".to_owned(),
                whole: true,
                passages: Vec::new(),
            }])
            .unwrap();
        let mut state = State::start(&path, Source::Compact, &[], soon()).unwrap();
        let again = state.working_set().unwrap().unwrap();
        assert_eq!((again.0, again.1.len()), (first.0, 1));
        assert_eq!(epoch_of(&path), 2);
        State::start(&path, Source::Clear, &[], soon()).unwrap();
        assert_eq!(epoch_of(&path), 3);
        let mut state = State::start(&path, Source::Compact, &[], soon()).unwrap();
        assert!(state.working_set().unwrap().is_none());
        assert_eq!(epoch_of(&path), 3);
    }

    /// How long a settle in these tests may wait for a lock it doesn't need.
    const BRIEF: Duration = Duration::from_millis(200);

    // A call that follows no hold settles without the state's write lock,
    // which another hook of the session may hold (#118): it never waits for
    // that lock, nor fails on it.
    #[test]
    fn a_call_after_no_hold_settles_without_the_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        State::start(&path, Source::Startup, &[], soon()).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut state = State::open(&path, Instant::now() + BRIEF).unwrap();
        state.settle(MAIN, "t-1", Some("k1")).unwrap();
        other.execute_batch("COMMIT").unwrap();
    }

    // A hold settles at its context's next tool call: overridden when that
    // call runs the held command again, heeded when it is anything else;
    // the held call itself and another context's calls settle nothing, and
    // the session sends each memory's latest outcome across its contexts
    // (#118).
    #[test]
    fn a_hold_settles_at_its_contexts_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let hold = |state: &mut State, agent: &str, id: &str, key: &str, memory: &str| {
            let call = Call { id, key };
            assert_eq!(
                state
                    .hold(agent, call, &[offer(memory, 0)], &[], ROOM_CHARS, soon())
                    .unwrap(),
                [0],
                "{agent} {memory}"
            );
        };
        let outcome = |state: &State, memory: &str| {
            let (unsent, _) = state.unsent().unwrap();
            unsent
                .into_iter()
                .find(|u| u.memory == memory)
                .map(|u| (u.held, u.overridden))
        };
        hold(&mut state, MAIN, "t-1", "k1", "e0000001");
        state.settle(MAIN, "t-1", Some("k1")).unwrap();
        state.settle("agent-1", "t-2", Some("k1")).unwrap();
        assert_eq!(outcome(&state, "e0000001"), Some((true, None)), "unsettled");
        state.settle(MAIN, "t-3", Some("k1")).unwrap();
        assert_eq!(outcome(&state, "e0000001"), Some((true, Some(true))));
        // Settled once: a later call changes nothing.
        state.settle(MAIN, "t-4", None).unwrap();
        assert_eq!(outcome(&state, "e0000001"), Some((true, Some(true))));
        hold(&mut state, MAIN, "t-5", "k2", "e0000002");
        state.settle(MAIN, "t-6", None).unwrap();
        assert_eq!(outcome(&state, "e0000002"), Some((true, Some(false))));
        // Held again in a subagent's context and heeded there: the latest.
        hold(&mut state, "agent-1", "t-7", "k1", "e0000001");
        state.settle("agent-1", "t-8", Some("k3")).unwrap();
        assert_eq!(outcome(&state, "e0000001"), Some((true, Some(false))));
    }

    #[test]
    fn a_startup_resets_the_file_another_hook_holds_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut first = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let note = [offer("e0000001", 0)];
        assert_eq!(
            first.record(MAIN, &command(), &note, ROOM_CHARS).unwrap(),
            [0]
        );
        drop(first);
        // Another hook of the session, its connection open across a startup
        // that reuses the session's id. Windows refuses to delete the file.
        let mut hook = State::open(&path, soon()).unwrap();
        let started = State::start(&path, Source::Startup, &[], soon()).unwrap();
        assert_eq!(started.window(MAIN).unwrap(), []);
        // The hook writes to the file the startup began anew, not to a
        // deleted one: the passage is shown again, and the step is there.
        assert_eq!(
            hook.record(MAIN, &command(), &note, ROOM_CHARS).unwrap(),
            [0]
        );
        assert_eq!(started.window(MAIN).unwrap().len(), 1);
    }

    #[test]
    fn a_startup_replaces_a_file_that_is_no_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "not a database, and long enough to have a header of sorts",
        )
        .unwrap();
        assert!(State::start(&path, Source::Resume, &[], soon()).is_err());
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000001", 0)], ROOM_CHARS)
                .unwrap(),
            [0]
        );
    }

    #[test]
    fn the_window_keeps_each_contexts_last_actions_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let mut failed = step(Kind::File, &["src", "pars"], &["src/parse.rs"]);
        failed.failed = true;
        let total = WINDOW_ACTIONS + 5;
        for i in 0..total {
            let mut prompt = step(Kind::Prompt, &[], &[]);
            prompt.terms.stems = BTreeSet::from([format!("word{i}")]);
            state.record(MAIN, &prompt, &[], ROOM_CHARS).unwrap();
        }
        state.record(MAIN, &failed, &[], ROOM_CHARS).unwrap();
        state
            .record("agent-1", &command(), &[], ROOM_CHARS)
            .unwrap();
        let window = state.window(MAIN).unwrap();
        assert_eq!(window.len(), WINDOW_ACTIONS);
        assert_eq!(window.last(), Some(&failed));
        let first = total + 1 - WINDOW_ACTIONS;
        assert_eq!(
            window[0].terms.stems,
            BTreeSet::from([format!("word{first}")])
        );
        assert_eq!(state.window("agent-1").unwrap(), [command()]);
        // Every kind is stored by a name it is read back by.
        for kind in [
            Kind::Prompt,
            Kind::Task,
            Kind::Plan,
            Kind::Command,
            Kind::File,
            Kind::Message,
            Kind::Search,
        ] {
            assert_eq!(kind_of(kind_name(kind)), Some(kind));
        }
    }

    #[test]
    fn each_subagent_claims_the_oldest_task_kept_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        let task = |anchor: &str| Terms {
            stems: BTreeSet::new(),
            anchors: BTreeSet::from([anchor.to_owned()]),
            ..Default::default()
        };
        state.spawn("p1\nExplore", &task("docs/notes.md")).unwrap();
        state.spawn("p1\nPlan", &task("src/parse.rs")).unwrap();
        state.spawn("p1\nExplore", &task("docs/guide.md")).unwrap();
        assert_eq!(
            state.claim("p1\nExplore").unwrap(),
            Some(task("docs/notes.md"))
        );
        assert_eq!(
            state.claim("p1\nExplore").unwrap(),
            Some(task("docs/guide.md"))
        );
        assert_eq!(state.claim("p1\nExplore").unwrap(), None);
        assert_eq!(state.claim("p2\nPlan").unwrap(), None);
        // Two subagents starting at once never claim one task twice.
        for i in 0..8 {
            state
                .spawn("p3\nExplore", &task(&format!("docs/{i}.md")))
                .unwrap();
        }
        let barrier = Arc::new(Barrier::new(8));
        let claimed: BTreeSet<String> = (0..8)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                thread::spawn(move || {
                    let mut state = State::open(&path, soon()).unwrap();
                    barrier.wait();
                    let terms = state.claim("p3\nExplore").unwrap().unwrap();
                    terms.anchors.into_iter().next().unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(claimed.len(), 8);
    }

    #[test]
    fn a_hook_out_of_time_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        State::start(&path, Source::Startup, &[], soon()).unwrap();
        let mut holder = State::open(&path, soon()).unwrap();
        let held = holder
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut late = State::open(&path, Instant::now()).unwrap();
        assert!(
            late.record(MAIN, &command(), &[offer("e0000001", 0)], ROOM_CHARS)
                .is_err()
        );
        held.commit().unwrap();
        let mut state = State::open(&path, soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap(), []);
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000001", 0)], ROOM_CHARS)
                .unwrap(),
            [0]
        );
    }

    /// Every layout before [`VERSION`], as the release that used it wrote
    /// the file, with one row in each table.
    const LAYOUTS: &[(i64, &str)] = &[
        (
            1,
            "CREATE TABLE contexts(
             agent TEXT PRIMARY KEY, epoch INTEGER NOT NULL,
             actions INTEGER NOT NULL, chars INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE window(
             agent TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL,
             failed INTEGER NOT NULL, stems TEXT NOT NULL, anchors TEXT NOT NULL,
             PRIMARY KEY(agent, seq)
         ) WITHOUT ROWID;
         CREATE TABLE spawns(
             seq INTEGER PRIMARY KEY, key TEXT NOT NULL, stems TEXT NOT NULL,
             anchors TEXT NOT NULL, claimed INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE shown(
             agent TEXT NOT NULL, memory TEXT NOT NULL, passage INTEGER NOT NULL,
             at INTEGER NOT NULL, PRIMARY KEY(agent, memory, passage)
         ) WITHOUT ROWID;
         INSERT INTO contexts VALUES ('main', 0, 3, 0);
         INSERT INTO window VALUES ('main', 1, 'command', 0, 'cargo', 'cargo build');
         INSERT INTO spawns(key, stems, anchors) VALUES ('k', 'cargo', 'cargo build');
         INSERT INTO shown VALUES ('main', 'e0000001', 0, 3);",
        ),
        (
            2,
            "CREATE TABLE contexts(
             agent TEXT PRIMARY KEY, epoch INTEGER NOT NULL,
             actions INTEGER NOT NULL, chars INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE window(
             agent TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL,
             failed INTEGER NOT NULL, stems TEXT NOT NULL, anchors TEXT NOT NULL,
             PRIMARY KEY(agent, seq)
         ) WITHOUT ROWID;
         CREATE TABLE spawns(
             seq INTEGER PRIMARY KEY, key TEXT NOT NULL, stems TEXT NOT NULL,
             anchors TEXT NOT NULL, claimed INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE shown(
             agent TEXT NOT NULL, memory TEXT NOT NULL, passage INTEGER NOT NULL,
             at INTEGER NOT NULL, PRIMARY KEY(agent, memory, passage)
         ) WITHOUT ROWID;
         CREATE TABLE held(
             agent TEXT NOT NULL, memory TEXT NOT NULL, passage INTEGER NOT NULL,
             at INTEGER NOT NULL, shown INTEGER NOT NULL,
             PRIMARY KEY(agent, memory, passage)
         ) WITHOUT ROWID;
         INSERT INTO contexts VALUES ('main', 0, 3, 0);
         INSERT INTO window VALUES ('main', 1, 'command', 0, 'cargo', 'cargo build');
         INSERT INTO spawns(key, stems, anchors) VALUES ('k', 'cargo', 'cargo build');
         INSERT INTO shown VALUES ('main', 'e0000001', 0, 3);
         INSERT INTO held VALUES ('main', 'e0000003', 0, 2, 0);",
        ),
        (
            3,
            "CREATE TABLE contexts(
             agent TEXT PRIMARY KEY, epoch INTEGER NOT NULL,
             actions INTEGER NOT NULL, chars INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE window(
             agent TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL,
             failed INTEGER NOT NULL, stems TEXT NOT NULL, anchors TEXT NOT NULL,
             PRIMARY KEY(agent, seq)
         ) WITHOUT ROWID;
         CREATE TABLE spawns(
             seq INTEGER PRIMARY KEY, key TEXT NOT NULL, stems TEXT NOT NULL,
             anchors TEXT NOT NULL, claimed INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE shown(
             agent TEXT NOT NULL, memory TEXT NOT NULL, passage INTEGER NOT NULL,
             at INTEGER NOT NULL, PRIMARY KEY(agent, memory, passage)
         ) WITHOUT ROWID;
         CREATE TABLE held(
             agent TEXT NOT NULL, memory TEXT NOT NULL, passage INTEGER NOT NULL,
             at INTEGER NOT NULL, shown INTEGER NOT NULL,
             PRIMARY KEY(agent, memory, passage)
         ) WITHOUT ROWID;
         CREATE TABLE worked(
             agent TEXT NOT NULL, memory TEXT NOT NULL,
             read INTEGER NOT NULL DEFAULT 0, recorded INTEGER NOT NULL DEFAULT 0,
             found INTEGER NOT NULL DEFAULT 0, shown INTEGER NOT NULL DEFAULT 0,
             held INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL,
             epoch INTEGER NOT NULL, sent INTEGER NOT NULL DEFAULT 0,
             changed INTEGER NOT NULL, PRIMARY KEY(agent, memory)
         ) WITHOUT ROWID;
         CREATE TABLE pushed(
             memory TEXT NOT NULL, passage INTEGER NOT NULL, times INTEGER NOT NULL,
             PRIMARY KEY(memory, passage)
         ) WITHOUT ROWID;
         CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
         INSERT INTO contexts VALUES ('main', 0, 3, 0);
         INSERT INTO window VALUES ('main', 1, 'command', 0, 'cargo', 'cargo build');
         INSERT INTO spawns(key, stems, anchors) VALUES ('k', 'cargo', 'cargo build');
         INSERT INTO shown VALUES ('main', 'e0000001', 0, 3);
         INSERT INTO held VALUES ('main', 'e0000003', 0, 2, 0);
         INSERT INTO worked(agent, memory, read, at, epoch, changed)
             VALUES ('main', 'e0000005', 1, 2, 0, 2);
         INSERT INTO pushed VALUES ('e0000001', 0, 1);
         INSERT INTO meta VALUES ('origin', 'claude:session/s1');",
        ),
        (
            4,
            "CREATE TABLE contexts(
    agent TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL,
    actions INTEGER NOT NULL,
    chars INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE window(
    agent TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    failed INTEGER NOT NULL,
    stems TEXT NOT NULL,
    anchors TEXT NOT NULL,
    PRIMARY KEY(agent, seq)
) WITHOUT ROWID;
CREATE TABLE spawns(
    seq INTEGER PRIMARY KEY,
    key TEXT NOT NULL,
    stems TEXT NOT NULL,
    anchors TEXT NOT NULL,
    claimed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE shown(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    passage INTEGER NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY(agent, memory, passage)
) WITHOUT ROWID;
CREATE TABLE held(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    passage INTEGER NOT NULL,
    at INTEGER NOT NULL,
    shown INTEGER NOT NULL,
    PRIMARY KEY(agent, memory, passage)
) WITHOUT ROWID;
CREATE TABLE worked(
    agent TEXT NOT NULL,
    memory TEXT NOT NULL,
    read INTEGER NOT NULL DEFAULT 0,
    recorded INTEGER NOT NULL DEFAULT 0,
    found INTEGER NOT NULL DEFAULT 0,
    shown INTEGER NOT NULL DEFAULT 0,
    held INTEGER NOT NULL DEFAULT 0,
    at INTEGER NOT NULL,
    epoch INTEGER NOT NULL,
    sent INTEGER NOT NULL DEFAULT 0,
    changed INTEGER NOT NULL,
    PRIMARY KEY(agent, memory)
) WITHOUT ROWID;
CREATE TABLE pushed(
    memory TEXT NOT NULL,
    passage INTEGER NOT NULL,
    times INTEGER NOT NULL,
    PRIMARY KEY(memory, passage)
) WITHOUT ROWID;
CREATE TABLE meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE tag_folders(
    folder TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    common TEXT NOT NULL,
    root TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE tag_pending(
    memory TEXT PRIMARY KEY,
    tag TEXT NOT NULL,
    project TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE tag_stamps(
    common TEXT NOT NULL,
    path TEXT NOT NULL,
    stamp TEXT NOT NULL,
    PRIMARY KEY(common, path)
) WITHOUT ROWID;
CREATE TABLE tag_ends(
    memory TEXT PRIMARY KEY,
    tag TEXT NOT NULL,
    head TEXT NOT NULL,
    project TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE tag_flags(
    flag TEXT PRIMARY KEY
) WITHOUT ROWID;
         INSERT INTO contexts VALUES ('main', 0, 3, 0);
         INSERT INTO window VALUES ('main', 1, 'command', 0, 'cargo', 'cargo build');
         INSERT INTO spawns(key, stems, anchors) VALUES ('k', 'cargo', 'cargo build');
         INSERT INTO shown VALUES ('main', 'e0000001', 0, 3);
         INSERT INTO held VALUES ('main', 'e0000003', 0, 2, 0);
         INSERT INTO worked(agent, memory, read, at, epoch, changed)
             VALUES ('main', 'e0000005', 1, 2, 0, 2);
         INSERT INTO pushed VALUES ('e0000001', 0, 1);
         INSERT INTO meta VALUES ('origin', 'claude:session/s1');
         INSERT INTO tag_flags VALUES ('checked');",
        ),
    ];

    /// The tables of `conn`'s file, each with its row count.
    fn tables(conn: &Connection) -> Vec<(String, i64)> {
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        names
            .into_iter()
            .map(|name| {
                let rows = conn
                    .query_row(&format!("SELECT count(*) FROM {name}"), [], |r| r.get(0))
                    .unwrap();
                (name, rows)
            })
            .collect()
    }

    #[test]
    fn an_older_layout_is_upgraded_in_place_keeping_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = file(&dir).with_extension("fresh");
        State::start(&fresh, Source::Startup, &[], soon()).unwrap();
        let current: Vec<String> = tables(&Connection::open(&fresh).unwrap())
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(LAYOUTS.iter().all(|(layout, _)| *layout < VERSION));
        assert_eq!(
            LAYOUTS.last().map(|(layout, _)| *layout),
            Some(VERSION - 1),
            "the layout before this one is listed"
        );
        for (layout, sql) in LAYOUTS {
            let path = file(&dir).with_extension(format!("v{layout}"));
            let old = Connection::open(&path).unwrap();
            old.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))
                .unwrap();
            old.execute_batch(sql).unwrap();
            old.pragma_update(None, "user_version", layout).unwrap();
            let rows = tables(&old);
            drop(old);
            let mut state = State::open(&path, soon()).unwrap();
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, VERSION, "layout {layout}");
            let now = tables(&conn);
            for row in &rows {
                assert!(now.contains(row), "layout {layout} kept {row:?}: {now:?}");
            }
            let names: Vec<&String> = now.iter().map(|(name, _)| name).collect();
            assert_eq!(names, current.iter().collect::<Vec<_>>(), "layout {layout}");
            assert!(
                state
                    .handled(MAIN, Use::Push)
                    .unwrap()
                    .contains("e0000001", 0),
                "layout {layout}"
            );
            assert_eq!(
                state
                    .hold(
                        MAIN,
                        Call {
                            id: "t-1",
                            key: "key"
                        },
                        &[offer("e0000002", 0)],
                        &[],
                        ROOM_CHARS,
                        soon()
                    )
                    .unwrap(),
                [0],
                "layout {layout}"
            );
            // A hold's outcome (#118) is kept in tables layout 5 added.
            state.settle(MAIN, "t-2", Some("key")).unwrap();
            let (unsent, _) = state.unsent().unwrap();
            assert!(
                unsent
                    .iter()
                    .any(|u| u.memory == "e0000002" && u.held && u.overridden == Some(true)),
                "layout {layout}"
            );
            // Use (#123) is kept in tables layout 3 added.
            state
                .used(MAIN, &["e0000004".to_owned()], How::Read)
                .unwrap();
            let (unsent, _) = state.unsent().unwrap();
            assert!(
                unsent.iter().any(|u| u.memory == "e0000004" && u.read),
                "layout {layout}"
            );
            // Tag ends (#121) are kept in tables layout 4 added.
            let project = "github.com/example/app";
            state
                .keep_ends(
                    project,
                    &[(
                        "e0000006".to_owned(),
                        "v1.0.0".to_owned(),
                        "1a2b3c4".to_owned(),
                    )],
                )
                .unwrap();
            assert_eq!(
                state.unwritten_ends().unwrap(),
                [(
                    project.to_owned(),
                    "e0000006".to_owned(),
                    "v1.0.0".to_owned(),
                    "1a2b3c4".to_owned()
                )],
                "layout {layout}"
            );
        }
    }

    #[test]
    fn a_file_of_another_layout_is_refused_until_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        State::start(&path, Source::Startup, &[], soon()).unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", VERSION + 1)
            .unwrap();
        assert!(State::open(&path, soon()).is_err());
        assert!(State::start(&path, Source::Resume, &[], soon()).is_err());
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000001", 0)], ROOM_CHARS)
                .unwrap(),
            [0]
        );
    }
}
