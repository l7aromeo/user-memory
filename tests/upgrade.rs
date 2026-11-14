//! `user-memory upgrade` on every platform, through the real binary: a
//! binary with no saved setup refuses and changes nothing, and `update`,
//! which corrects a memory, names `upgrade` when typed with no id. The
//! upgrades themselves run in tests/install.rs (Unix) and
//! tests/windows_setup.rs (Windows).

#[path = "support/sandbox.rs"]
mod sandbox;

use sandbox::Sandboxed;
use std::{fs, process::Command};
use tempfile::tempdir;

#[test]
fn a_binary_without_a_saved_setup_is_not_upgraded() {
    let root = tempdir().unwrap();
    let binary = root
        .path()
        .join(format!("user-memory{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_user-memory"), &binary).unwrap();
    let before = fs::read(&binary).unwrap();
    for args in [
        &["upgrade"][..],
        &["upgrade", "--yes"],
        &["upgrade", "--check"],
    ] {
        let out = Command::new(&binary)
            .args(args)
            .sandboxed(root.path())
            .env("USER_MEMORY_RELEASES", "http://127.0.0.1:9/releases")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(
                "This user-memory wasn't installed by the installer, so upgrade can't tell how to upgrade it; upgrade it the way it was installed."
            ),
            "{args:?}: {out:?}"
        );
    }
    assert_eq!(fs::read(&binary).unwrap(), before);
    let mut left: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    left.retain(|name| name != binary.file_name().unwrap());
    assert!(
        left.iter()
            .all(|name| name.to_string_lossy().starts_with('.')),
        "{left:?}"
    );
}

/// `update` corrects a memory; typed with no id it fails as before, with
/// clap's usage error and code, and names the command that upgrades.
#[test]
fn update_with_no_id_names_upgrade_and_keeps_its_exit_code() {
    let root = tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("update")
        .sandboxed(root.path())
        .env("USER_MEMORY_DB", root.path().join("memories.sqlite3"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("the following required arguments were not provided"),
        "{stderr}"
    );
    assert!(
        stderr.ends_with("\nTo move user-memory to a newer release, run user-memory upgrade.\n"),
        "{stderr}"
    );
    // Other commands' usage errors stay as they were.
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("read")
        .arg("--nonsense")
        .sandboxed(root.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("user-memory upgrade"));
}
