//! The sandbox every test runs user-memory in holds all of its folders, on
//! every platform, so no test reaches the profile of the machine running
//! it: its real store, rules or PATH. The debug-only command that says
//! where they are never reaches a release build.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;

use serde_json::Value;
use std::{fs, path::Path, process::Command};

// Every folder user-memory resolves, and its default store, lie inside the
// sandbox: HOME and XDG on Unix, USERPROFILE and LOCALAPPDATA on Windows.
#[test]
fn a_sandboxed_command_keeps_every_folder_inside_its_sandbox() {
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("debug-paths")
        .sandboxed(root.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let folders: Value = serde_json::from_slice(&out.stdout).unwrap();
    let folders = folders.as_object().unwrap();
    assert_eq!(
        folders.keys().map(String::as_str).collect::<Vec<_>>(),
        ["home", "data", "state", "config", "bin", "docker", "store"]
    );
    for (name, path) in folders {
        let path = Path::new(path.as_str().unwrap_or_else(|| panic!("no {name}")));
        assert!(
            path.starts_with(root.path()),
            "{name} is {}, outside the sandbox {}",
            path.display(),
            root.path().display()
        );
    }
}

// The command is declared and handled only under cfg(debug_assertions),
// so a release build has neither: the release workflow also runs each
// release binary and requires it to refuse the command.
#[test]
fn only_debug_builds_have_the_paths_command() {
    let main =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")).unwrap();
    let lines: Vec<&str> = main.lines().collect();
    let uses: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("DebugPaths"))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(uses.len(), 2, "the variant and its match arm");
    for at in uses {
        // The attributes and doc lines right above the line.
        let above = lines[..at]
            .iter()
            .rev()
            .take_while(|line| {
                let line = line.trim_start();
                line.starts_with("#[") || line.starts_with("///")
            })
            .any(|line| line.trim() == "#[cfg(debug_assertions)]");
        assert!(above, "line {} lacks #[cfg(debug_assertions)]", at + 1);
    }
}
