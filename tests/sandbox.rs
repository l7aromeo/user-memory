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
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

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

/// Every `.rs` file under `src`, with its text.
fn sources() -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let mut folders = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(folder).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path).unwrap();
                found.push((path, text));
            }
        }
    }
    found
}

/// Whether the attributes and doc lines right above line `at` include
/// `#[cfg(debug_assertions)]`.
fn debug_only(lines: &[&str], at: usize) -> bool {
    lines[..at]
        .iter()
        .rev()
        .take_while(|line| {
            let line = line.trim_start();
            line.starts_with("#[") || line.starts_with("///")
        })
        .any(|line| line.trim() == "#[cfg(debug_assertions)]")
}

// The tests' seams change what a build reports or does, so a release build
// must have none: each environment variable is named once, by a constant
// declared only under cfg(debug_assertions), and the test-only tool and
// its router likewise.
#[test]
fn only_debug_builds_have_the_test_seams() {
    let sources = sources();
    for seam in [
        "USER_MEMORY_TEST_VERSION",
        "USER_MEMORY_TEST_SCHEMA_VERSION",
        "USER_MEMORY_TEST_RELAY",
        "USER_MEMORY_TEST_EXEC_FAIL",
        "USER_MEMORY_TEST_TRACE",
        "USER_MEMORY_TEST_LINKS",
        "USER_MEMORY_HOLD_SNAPSHOT",
        "USER_MEMORY_HOLD_ABANDON",
    ] {
        let quoted = format!("\"{seam}\"");
        let uses: Vec<(&PathBuf, usize)> = sources
            .iter()
            .flat_map(|(path, text)| {
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| line.contains(&quoted))
                    .map(move |(at, _)| (path, at))
            })
            .collect();
        assert_eq!(uses.len(), 1, "{seam} is named in {uses:?}");
        let (path, at) = uses[0];
        let text = &sources.iter().find(|(p, _)| p == path).unwrap().1;
        let lines: Vec<&str> = text.lines().collect();
        let declared = lines[at].trim_start().starts_with("const ")
            || lines[at].trim_start().starts_with("pub(crate) const ");
        assert!(
            debug_only(&lines, at) && declared,
            "{seam} at {}:{} is not a constant under #[cfg(debug_assertions)]",
            path.display(),
            at + 1
        );
    }
    for (marker, count) in [("debug_router", 3)] {
        let mut seen = 0;
        for (path, text) in &sources {
            let lines: Vec<&str> = text.lines().collect();
            for (at, line) in lines.iter().enumerate() {
                if line.contains(marker) && !line.trim_start().starts_with("//") {
                    seen += 1;
                    // The attribute, or the statement holding the call.
                    let guarded = debug_only(&lines, at)
                        || lines[at.saturating_sub(2)..at]
                            .iter()
                            .any(|line| line.trim() == "#[cfg(debug_assertions)]");
                    assert!(
                        guarded,
                        "{marker} at {}:{} is not under #[cfg(debug_assertions)]",
                        path.display(),
                        at + 1
                    );
                }
            }
        }
        assert_eq!(seen, count, "{marker}");
    }
}
