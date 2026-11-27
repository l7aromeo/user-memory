//! Waiting in tests without wall-clock bounds: a test checks what happened,
//! and time only cuts off a hang.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    process::{Child, Command, Output},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// How long a test waits for something that comes at once unless the code
/// is stuck. Only a hang should reach it; a slow, loaded machine must not.
pub const STUCK: Duration = Duration::from_secs(60);

/// The output of `child` once it exits, returned as soon as it exits; past
/// [`STUCK`] it is killed and the test fails, naming `what` hung.
///
/// A thread blocks on the exit and reads the output meanwhile, so a test
/// that times the child measures the child. Polling would add up to one
/// poll interval, and the OS stretches a polling sleep: at macOS's
/// background priority a 20 ms sleep takes about 200 ms. Its stdin, if
/// piped and still open, stays open until the exit, as a client holding the
/// pipe keeps it. It returns once stdout and stderr close too, so a child
/// whose own child inherits them is timed to that grandchild's end.
pub fn finish(child: Child, what: &str) -> Output {
    finish_within(child, what, STUCK)
}

/// [`finish`], with `limit` in place of [`STUCK`]: for the tests of
/// `finish` itself.
pub fn finish_within(mut child: Child, what: &str, limit: Duration) -> Output {
    let id = child.id();
    let input = child.stdin.take();
    let (sent, exited) = mpsc::channel();
    thread::spawn(move || {
        let _ = sent.send(child.wait_with_output());
    });
    let Ok(output) = exited.recv_timeout(limit) else {
        kill(id);
        panic!("{what} hung");
    };
    drop(input);
    output.unwrap()
}

/// Kills the process `id`, which [`finish`]'s waiting thread owns. std's
/// `Child::kill` needs the `Child`, and `wait_with_output` holds it until
/// the exit, so it is killed by its id, as a user would.
fn kill(id: u32) {
    let id = id.to_string();
    let _ = if cfg!(windows) {
        Command::new("taskkill").args(["/F", "/PID", &id]).output()
    } else {
        Command::new("kill").args(["-9", &id]).output()
    };
}

/// Waits until `done` holds; past [`STUCK`] the test fails, naming `what`
/// never happened.
pub fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + STUCK;
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The environment a hook runs in when a test checks what it prints: its
/// time limits ten times longer, so a loaded machine never cuts it short,
/// yet its 5-second limit still ends before [`STUCK`].
pub const HOOK_ROOM: (&str, &str) = ("USER_MEMORY_HOOK_TIME_SCALE", "10");
/// The hook's own time limits, for the tests of what happens at them.
pub const HOOK_LIMITS: (&str, &str) = ("USER_MEMORY_HOOK_TIME_SCALE", "1");

/// How far ahead the first round of [`date_window`] sets its date: enough
/// for a record, a session start and one hook on a fast machine.
const DATE_AHEAD: Duration = Duration::from_secs(4);

/// How many rounds [`date_window`] tries, the margin doubling each time:
/// 4, 8 and 16 s, so a slow machine costs at most about half a minute more.
const DATE_ROUNDS: u32 = 3;

/// What `record` says when a memory's date passed before the record made
/// it: on a slow machine, a round that took its whole margin to get there.
const DATE_PASSED: &str = "until's date has already passed";

/// One round of [`date_window`]: a date a margin ahead of when the round
/// first asks for it, and how long after that its first check came.
pub struct DateRound {
    margin: Duration,
    set: std::cell::OnceCell<(jiff::Timestamp, Instant)>,
    checked: std::cell::Cell<Option<Duration>>,
}

impl DateRound {
    /// The round's date and when it was set, setting both on the first ask:
    /// the margin counts from the record that takes the date, not from the
    /// setup before it.
    fn date(&self) -> (jiff::Timestamp, Instant) {
        *self.set.get_or_init(|| {
            let ahead = i64::try_from(self.margin.as_secs()).expect("a margin of seconds fits");
            let until = jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + ahead)
                .expect("a date seconds ahead of now is in range");
            (until, Instant::now())
        })
    }

    /// The round's date as `record --until-date` and the tools take it, in
    /// whole seconds, so it is never later than the margin.
    pub fn until(&self) -> String {
        self.date().0.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// The id `record` replied with, or none when the date passed before
    /// the record was made, which says nothing and tries the round again.
    /// Any other refusal fails the test.
    pub fn stored(&self, reply: &serde_json::Value) -> Option<String> {
        if reply["stored"] == true {
            return Some(
                reply["id"]
                    .as_str()
                    .expect("a stored memory's id")
                    .to_owned(),
            );
        }
        let passed = reply["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains(DATE_PASSED));
        assert!(passed && !self.still_ahead(), "{reply}");
        None
    }

    /// Whether the round's first check, which found the memory `pushed` or
    /// not, tells anything. A push does. No push while the date was still
    /// ahead fails the test, since the memory was in force; no push once it
    /// had passed says nothing, and the round is tried again.
    pub fn ahead(&self, pushed: bool) -> bool {
        let ahead = self.still_ahead();
        assert!(
            pushed || !ahead,
            "not pushed while its date was still ahead"
        );
        pushed
    }

    /// Whether the round's date is still ahead now, after the checks that
    /// need it ahead; a round whose date passed first is tried again.
    pub fn still_ahead(&self) -> bool {
        let (until, set) = self.date();
        self.checked.set(Some(set.elapsed()));
        jiff::Timestamp::now() < until
    }

    /// Waits until the round's date has passed, and a second more.
    pub fn wait_past(&self) {
        // A date already past leaves nothing to wait for but the second.
        let left = Duration::try_from(jiff::Timestamp::now().duration_until(self.date().0))
            .unwrap_or_default();
        thread::sleep(left + Duration::from_secs(1));
    }
}

/// Runs `round` with a date a margin ahead, for a test of a memory whose
/// date passes while it is in use: the round records the memory with
/// [`DateRound::until`] and [`DateRound::stored`], checks it is pushed with
/// [`DateRound::ahead`], waits past the date and checks it is not. A round
/// whose date passed before its first check, as when a slow machine took
/// the whole margin, is tried again with twice the margin; a wrong result
/// never is.
pub fn date_window(mut round: impl FnMut(&DateRound) -> bool) {
    let mut margin = DATE_AHEAD;
    let mut checked = Duration::ZERO;
    for _ in 0..DATE_ROUNDS {
        let date = DateRound {
            margin,
            set: std::cell::OnceCell::new(),
            checked: std::cell::Cell::new(None),
        };
        if round(&date) {
            return;
        }
        checked = date.checked.get().unwrap_or_default();
        eprintln!(
            "a date {margin:?} ahead passed before the first check, which came {checked:?} after it was set; trying again"
        );
        margin *= 2;
    }
    panic!(
        "the date passed before the first check in all {DATE_ROUNDS} rounds: the last was {:?} ahead, and its check came {checked:?} after it was set",
        margin / 2
    );
}
