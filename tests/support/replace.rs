//! Replaces a running binary as an upgrade does (`replace_aside` in
//! src/setup.rs): on Windows, which can't replace a running program, the
//! running one is renamed aside first, then the staged one takes its place;
//! each rename waits out a passing hold as `replace_aside`'s does.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// How long a rename waits for another program to let go of the file: the
/// bound `replace_aside` waits (`RENAME_WAIT` in src/setup.rs). Another
/// program may open a new or just-started executable for a moment without
/// sharing delete, and a rename fails with a sharing violation meanwhile.
const RENAME_WAIT: Duration = Duration::from_secs(5);

/// The pause between two tries of a refused rename.
const RENAME_PAUSE: Duration = Duration::from_millis(10);

/// ERROR_SHARING_VIOLATION, Windows' error 32.
const SHARING_VIOLATION: i32 = 32;

/// Renames `from` to `to`; on Windows it tries again for up to
/// [`RENAME_WAIT`] while another program has either file open without
/// sharing delete. A hold past the wait fails the test with error 32.
pub fn rename(from: &Path, to: &Path) {
    let deadline = Instant::now() + RENAME_WAIT;
    loop {
        match fs::rename(from, to) {
            Err(err)
                if cfg!(windows)
                    && err.raw_os_error() == Some(SHARING_VIOLATION)
                    && Instant::now() < deadline =>
            {
                thread::sleep(RENAME_PAUSE);
            }
            done => return done.unwrap(),
        }
    }
}

/// Puts `staged` in place of the running `binary` as an upgrade does, the
/// running one renamed to `aside` first on Windows.
pub fn replace_running(binary: &Path, staged: &Path, aside: &Path) {
    if cfg!(windows) {
        rename(binary, aside);
    }
    rename(staged, binary);
}
