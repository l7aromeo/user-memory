//! Recall through the task (#117): what each agent context of a session
//! did lately and what was pushed to it, in one SQLite file beside the
//! session's snapshot.
//!
//! - The window: each action's terms (stems and anchors, never a tool's
//!   result or a file's contents), the last [`WINDOW_ACTIONS`] per context.
//! - `shown`: the passages pushed to a context in its current epoch, each
//!   with the action it was pushed at, so a passage is pushed once per
//!   stretch ([`STRETCH_ACTIONS`]) and two hooks at once push it once.
//! - The epoch: `startup` opens the first, `clear` and `compact` a new one
//!   with its own budget ([`SESSION_PUSH_CHARS`]); `resume` keeps it.
//!
//! A hook reads the window outside any lock, matches, then writes its
//! action and what it pushes in one short `BEGIN IMMEDIATE` transaction,
//! so the shown-once rule and the budget hold however many hooks run.

use super::live::RECALL;
use crate::{
    memory::{
        matcher::{Kind, Terms, WINDOW_ACTIONS},
        push::PUSH_PER_EVENT,
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

/// The state file's layout. Another layout is refused, never read: the
/// file is rebuilt at the session's next `startup`.
const VERSION: i64 = 1;

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
";

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

/// A passage a hook would push, with the characters its line takes.
#[derive(Clone, Copy, Debug)]
pub(super) struct Offer<'a> {
    pub(super) memory: &'a str,
    pub(super) passage: i64,
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
    /// and keeps the window, as the work goes on; `resume` keeps the epoch
    /// and what it showed.
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
        if matches!(source, Source::Clear | Source::Compact) {
            if source == Source::Clear {
                tx.execute("DELETE FROM window WHERE agent = ?1", [MAIN])?;
            }
            tx.execute("DELETE FROM shown WHERE agent = ?1", [MAIN])?;
            tx.execute(
                "UPDATE contexts SET epoch = epoch + 1, chars = 0 WHERE agent = ?1",
                [MAIN],
            )?;
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
    /// their positions in `offers`. All or nothing: a hook that cannot write
    /// before its deadline pushes nothing and loses its step.
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
        let (_, actions, mut chars) = context(&tx, agent)?;
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
            let whole: bool = tx
                .query_row(
                    "SELECT 1 FROM shown WHERE agent = ?1 AND memory = substr(?2, 1, length(memory)) AND passage = ?3 AND ?4 - at <= ?5",
                    params![agent, offer.memory, WHOLE, at, STRETCH_ACTIONS],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if whole {
                continue;
            }
            let marked = tx.execute(
                "INSERT INTO shown(agent, memory, passage, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent, memory, passage) DO UPDATE SET at = excluded.at
                 WHERE excluded.at - shown.at > ?5",
                params![agent, offer.memory, offer.passage, at, STRETCH_ACTIONS],
            )?;
            if marked == 1 {
                chars += offer.chars;
                event += offer.chars;
                shown.push(position);
            }
        }
        tx.execute(
            "UPDATE contexts SET actions = ?2, chars = ?3 WHERE agent = ?1",
            params![agent, at, i64::try_from(chars)?],
        )?;
        tx.commit()?;
        Ok(shown)
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

/// `agent`'s context row, made when missing: its epoch, the actions it has
/// taken and the characters pushed to it in this epoch.
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
                chars: 151,
            },
            Offer {
                memory: "e0000008",
                passage: 0,
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
