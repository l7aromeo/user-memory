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
