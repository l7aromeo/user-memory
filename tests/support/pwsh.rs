//! The PowerShell a test runs: whether it must run, whether it does, and
//! how a run of it ended. A failure names the exit code, or on Unix the
//! signal that ended it, with what it printed, so a PowerShell that crashes
//! says how.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::process::{Command, Output};

/// The arguments of the probe: a PowerShell that runs at all exits 0.
pub const PROBE: [&str; 4] = ["-NoProfile", "-NonInteractive", "-Command", "exit 0"];

/// Whether PowerShell must run rather than skip: in CI (CI set, as GitHub
/// sets it), or with REQUIRE_PWSH set.
pub fn required() -> bool {
    let set = |name| std::env::var(name).is_ok_and(|value| !value.is_empty() && value != "false");
    set("CI") || set("REQUIRE_PWSH")
}

/// Why `program` does not run, if it does not: the probe's start error, or
/// how it ended and what it printed.
pub fn probe(program: &str) -> Result<(), String> {
    let shown = format!("`{program} {}`", PROBE.join(" "));
    match Command::new(program).args(PROBE).output() {
        Err(error) => Err(format!("{shown} does not start: {error}")),
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!("{shown} {}", ended(&out))),
    }
}

/// Whether `program` runs; a test returns early without it. Where it is
/// required, its absence fails the test, saying why; elsewhere a note says
/// what is skipped.
#[track_caller]
pub fn available(program: &str, skipped: &str) -> bool {
    let Err(why) = probe(program) else {
        return true;
    };
    assert!(
        !required(),
        "{program} is required here (CI or REQUIRE_PWSH is set), but it does not run: {why}"
    );
    eprintln!("{program} does not run here, so {skipped} are skipped: {why}");
    false
}

/// Fails the test, naming `what`, unless `out` exited with `code`.
#[track_caller]
pub fn assert_exit(out: &Output, code: i32, what: &str) {
    assert!(
        out.status.code() == Some(code),
        "{what}: expected exit code {code}, but it {}",
        ended(out)
    );
}

/// How a run ended and what it printed: `ExitStatus` names the exit code,
/// or on Unix the signal, as "exit status: 1" or "signal: 6 (SIGABRT)".
fn ended(out: &Output) -> String {
    format!(
        "ended with {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout).trim_end(),
        String::from_utf8_lossy(&out.stderr).trim_end()
    )
}
