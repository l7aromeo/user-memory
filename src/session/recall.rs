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
    memory::matcher::{Kind, Terms, WINDOW_ACTIONS},
    private,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::BTreeSet,
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
/// prints a rule in full: every passage of it counts as shown.
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
        let dir = path.parent().context("a state file has a folder")?;
        private::create_dir(dir)?;
        drop(private::create_file(
            fs::OpenOptions::new().write(true).create(true),
            path,
        )?);
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
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
    /// shown from now. A `startup` begins a new file; `clear` empties the
    /// window and opens an epoch; `compact` opens one and keeps the window,
    /// as the work goes on; `resume` keeps the epoch and what it showed.
    pub(super) fn start(
        path: &Path,
        source: Source,
        whole: &[&str],
        deadline: Instant,
    ) -> Result<Self> {
        if source == Source::Startup {
            for file in [path.to_owned(), wal(path), shm(path)] {
                match fs::remove_file(&file) {
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                        return Err(err)
                            .with_context(|| format!("Cannot remove {}", file.display()));
                    }
                    _ => {}
                }
            }
        }
        let mut state = Self::open(path, deadline)?;
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
        for memory in whole {
            tx.execute(
                "INSERT OR REPLACE INTO shown(agent, memory, passage, at) VALUES (?1, ?2, ?3, ?4)",
                params![MAIN, memory, WHOLE, actions],
            )?;
        }
        tx.commit()?;
        Ok(state)
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
                },
            });
        }
        Ok(steps)
    }

    /// Adds `step` to `agent`'s window and marks as shown those of `offers`
    /// not shown in this stretch, in order, while the epoch's budget holds;
    /// returns their positions in `offers`. All or nothing: a hook that
    /// cannot write before its deadline pushes nothing and loses its step.
    pub(super) fn record(
        &mut self,
        agent: &str,
        step: &Step,
        offers: &[Offer],
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
        for (position, offer) in offers.iter().enumerate() {
            if chars + offer.chars > SESSION_PUSH_CHARS {
                continue;
            }
            let whole: bool = tx
                .query_row(
                    "SELECT 1 FROM shown WHERE agent = ?1 AND memory = ?2 AND passage = ?3 AND ?4 - at <= ?5",
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
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    /// Room for a test's hook on a loaded machine.
    const ROOM: Duration = Duration::from_secs(30);

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
        assert_eq!(state.record(MAIN, &command(), &offers).unwrap(), [0, 1]);
        assert_eq!(
            state.record(MAIN, &command(), &offers).unwrap(),
            [] as [usize; 0]
        );
        // A subagent hasn't seen what its parent saw.
        assert_eq!(
            state.record("agent-1", &command(), &offers).unwrap(),
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
                                .record(MAIN, &command(), &[offer(&memory, 0)])
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
                            state.record(MAIN, &command(), &offers).unwrap().len()
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
        assert_eq!(state.record(MAIN, &command(), &offers).unwrap(), [0, 2]);
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000004", 0)])
                .unwrap(),
            [] as [usize; 0]
        );
        // Another context has its own budget.
        assert_eq!(
            state
                .record("agent-1", &command(), &[big("e0000002")])
                .unwrap(),
            [0]
        );
        for source in [Source::Clear, Source::Compact] {
            let mut state = State::start(&path, source, &[], soon()).unwrap();
            assert_eq!(
                state.record(MAIN, &command(), &[big("e0000005")]).unwrap(),
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
            state.record(MAIN, &command(), &[over]).unwrap(),
            [] as [usize; 0]
        );
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000006", 0)])
                .unwrap(),
            [0]
        );
    }

    #[test]
    fn a_passage_shown_more_than_stretch_actions_ago_is_shown_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &["e0000009"], soon()).unwrap();
        let rule = [offer("e0000001", 2)];
        let printed = [offer("e0000009", 0)];
        assert_eq!(state.record(MAIN, &command(), &rule).unwrap(), [0]);
        // Shown at action 1; actions 2 to 1 + STRETCH_ACTIONS are within it.
        for _ in 1..STRETCH_ACTIONS {
            state.record(MAIN, &command(), &[]).unwrap();
        }
        assert_eq!(
            state.record(MAIN, &command(), &rule).unwrap(),
            [] as [usize; 0]
        );
        assert_eq!(state.record(MAIN, &command(), &rule).unwrap(), [0]);
        assert_eq!(
            state.record(MAIN, &command(), &rule).unwrap(),
            [] as [usize; 0]
        );
        // A memory session start printed whole, at action 0, likewise.
        assert_eq!(state.record(MAIN, &command(), &printed).unwrap(), [0]);
        // What a resume prints whole counts as shown from then.
        let mut state = State::start(&path, Source::Resume, &["e0000008"], soon()).unwrap();
        let reprinted = [offer("e0000008", 1)];
        assert_eq!(
            state.record(MAIN, &command(), &reprinted).unwrap(),
            [] as [usize; 0]
        );
    }

    #[test]
    fn session_start_opens_epochs_as_the_acceptance_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut state = State::start(&path, Source::Startup, &["e0000009"], soon()).unwrap();
        let both = [offer("e0000001", 0), offer("e0000002", 0)];
        assert_eq!(state.record(MAIN, &command(), &both).unwrap(), [0, 1]);
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000009", 3)])
                .unwrap(),
            [] as [usize; 0],
            "printed whole at session start"
        );
        state.record("agent-1", &command(), &both).unwrap();

        // resume: nothing shown before is pushed again.
        let mut state = State::start(&path, Source::Resume, &[], soon()).unwrap();
        assert_eq!(
            state.record(MAIN, &command(), &both).unwrap(),
            [] as [usize; 0]
        );
        assert_eq!(state.window(MAIN).unwrap().len(), 3);

        // compact: the window goes on; only what session start restored
        // counts as shown.
        let mut state = State::start(&path, Source::Compact, &["e0000001"], soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap().len(), 3);
        assert_eq!(state.record(MAIN, &command(), &both).unwrap(), [1]);
        // A subagent's context is its own.
        assert_eq!(
            state.record("agent-1", &command(), &both).unwrap(),
            [] as [usize; 0]
        );

        // clear: an empty window, and every passage can be pushed again.
        let mut state = State::start(&path, Source::Clear, &[], soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap(), []);
        assert_eq!(state.record(MAIN, &command(), &both).unwrap(), [0, 1]);

        // A new session, or a startup that reuses an id: no rows.
        let mut state = State::start(&path, Source::Startup, &[], soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap(), []);
        assert_eq!(state.window("agent-1").unwrap(), []);
        assert_eq!(state.record(MAIN, &command(), &both).unwrap(), [0, 1]);
        let other = path.with_file_name("claude-s2-key.recall");
        let mut state = State::start(&other, Source::Startup, &[], soon()).unwrap();
        assert_eq!(state.record(MAIN, &command(), &both).unwrap(), [0, 1]);
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
            state.record(MAIN, &prompt, &[]).unwrap();
        }
        state.record(MAIN, &failed, &[]).unwrap();
        state.record("agent-1", &command(), &[]).unwrap();
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
            late.record(MAIN, &command(), &[offer("e0000001", 0)])
                .is_err()
        );
        held.commit().unwrap();
        let mut state = State::open(&path, soon()).unwrap();
        assert_eq!(state.window(MAIN).unwrap(), []);
        assert_eq!(
            state
                .record(MAIN, &command(), &[offer("e0000001", 0)])
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
                .record(MAIN, &command(), &[offer("e0000001", 0)])
                .unwrap(),
            [0]
        );
    }
}
