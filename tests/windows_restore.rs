//! The restore commands `backup list` prints on Windows run as printed, in
//! PowerShell 7 (`pwsh`) and in Windows PowerShell 5.1 (`powershell`), on a
//! store whose path holds the characters PowerShell quoting has to handle.
//! In CI (CI set, as GitHub sets it) or with REQUIRE_PWSH set, `pwsh` is
//! required; elsewhere its run is skipped without it, with a note. Windows
//! PowerShell is part of every Windows.

#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// Whether `pwsh` must run rather than skip: in CI, or with REQUIRE_PWSH
/// set.
fn required() -> bool {
    let set = |name| std::env::var(name).is_ok_and(|value| !value.is_empty() && value != "false");
    set("CI") || set("REQUIRE_PWSH")
}

fn runs(shell: &str) -> bool {
    Command::new(shell)
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .output()
        .is_ok_and(|out| out.status.success())
}

fn said(out: &Output) -> String {
    format!(
        "{}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn user_memory(root: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(args)
        .sandboxed(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "user-memory {args:?}: {}", said(&out));
    out
}

fn record(root: &Path, content: &str) {
    user_memory(
        root,
        &[
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--scope",
            "global",
            "--content",
            content,
        ],
    );
}

/// Runs `commands` in `shell` as a script, stopping at the first error, as
/// a user pasting them would see it.
fn run(shell: &str, dir: &Path, commands: &[String]) -> Output {
    let script = dir.join("restore.ps1");
    // With a byte order mark, so Windows PowerShell reads it as UTF-8.
    let text = format!(
        "\u{feff}$ErrorActionPreference = 'Stop'\r\n{}\r\n",
        commands.join("\r\n")
    );
    fs::write(&script, text).unwrap();
    Command::new(shell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .output()
        .unwrap()
}

fn log(store: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", store.display()))
}

// The printed commands delete the store's log files, which may or may not
// be there, and copy the newest backup over the store: afterwards the store
// is that backup, byte for byte, in both PowerShells.
#[test]
fn the_restore_commands_run_in_both_powershells() {
    for shell in ["powershell", "pwsh"] {
        if !runs(shell) {
            assert!(
                shell == "pwsh" && !required(),
                "{shell} is required here, but it does not run"
            );
            eprintln!("{shell} is not installed: skipping its restore run");
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        // A quote of each kind PowerShell reads as one, a space and
        // wildcard brackets.
        let root = dir.path().join("it's \u{2019}here [1]");
        fs::create_dir(&root).unwrap();
        record(&root, "Kept from before the backup");
        user_memory(&root, &["backup", "now"]);
        record(&root, "Recorded after the backup");
        let store = sandbox::folder(&root, "store");
        let backups: Vec<PathBuf> = fs::read_dir(store.parent().unwrap().join("backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        let listed = user_memory(&root, &["backup", "list"]);
        let listed = String::from_utf8(listed.stdout).unwrap();
        let commands: Vec<String> = listed
            .lines()
            .skip_while(|line| !line.starts_with("To restore one"))
            .skip(1)
            .map(|line| line.trim().to_owned())
            .collect();
        assert_eq!(commands.len(), 2, "{listed}");
        assert_ne!(fs::read(&store).unwrap(), fs::read(&backups[0]).unwrap());
        // Logs a crash left, and then none: both must work.
        for leftover in [true, false] {
            if leftover {
                fs::write(log(&store, "wal"), "stale").unwrap();
                fs::write(log(&store, "shm"), "stale").unwrap();
            }
            let out = run(shell, dir.path(), &commands);
            assert!(
                out.status.success() && out.stderr.is_empty(),
                "{shell}, logs left: {leftover}: {commands:#?}\n{}",
                said(&out)
            );
            assert!(!log(&store, "wal").exists(), "{shell}: -wal is still there");
            assert!(!log(&store, "shm").exists(), "{shell}: -shm is still there");
            assert_eq!(
                fs::read(&store).unwrap(),
                fs::read(&backups[0]).unwrap(),
                "{shell}: the store is not the backup"
            );
        }
    }
}
