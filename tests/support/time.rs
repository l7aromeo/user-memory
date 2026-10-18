//! Waiting in tests without wall-clock bounds: a test checks what happened,
//! and time only cuts off a hang.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    process::{Child, Output},
    time::{Duration, Instant},
};

/// How long a test waits for something that comes at once unless the code
/// is stuck. Only a hang should reach it; a slow, loaded machine must not.
pub const STUCK: Duration = Duration::from_secs(60);

/// The output of `child` once it exits; past [`STUCK`] it is killed and the
/// test fails, naming `what` hung. Its output must fit the pipe buffers
/// (64 KiB on Linux and macOS), as it is read only after the exit.
pub fn finish(mut child: Child, what: &str) -> Output {
    let deadline = Instant::now() + STUCK;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} hung");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// The environment a hook runs in when a test checks what it prints: its
/// time limits ten times longer, so a loaded machine never cuts it short,
/// yet its 5-second limit still ends before [`STUCK`].
pub const HOOK_ROOM: (&str, &str) = ("USER_MEMORY_HOOK_TIME_SCALE", "10");
/// The hook's own time limits, for the tests of what happens at them.
pub const HOOK_LIMITS: (&str, &str) = ("USER_MEMORY_HOOK_TIME_SCALE", "1");
