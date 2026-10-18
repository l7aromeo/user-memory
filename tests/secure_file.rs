//! The `secure-file` command, which the Windows installer runs on the
//! token file it saves, so the script itself needs no access-list code, and
//! that doctor names when a file of user-memory's is open to others.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use std::{fs, path::Path, process::Command};

fn secure_file(home: &Path, path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("secure-file")
        .arg(path)
        .sandboxed(home)
        .env("USER_MEMORY_DB", home.join("memories.sqlite3"))
        .env_remove("USER_MEMORY_URL")
        .output()
        .unwrap()
}

#[test]
fn secure_file_makes_a_file_private_and_names_one_that_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("client-token");
    fs::write(&token, "secret").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let out = secure_file(dir.path(), &token);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&token).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert_eq!(fs::read_to_string(&token).unwrap(), "secret");

    let missing = dir.path().join("missing");
    let out = secure_file(dir.path(), &missing);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("Cannot make {} private", missing.display())),
        "{stderr}"
    );
    assert!(
        !dir.path().join("memories.sqlite3").exists(),
        "no store is made"
    );
}

// A folder is made private too, mode 0700 on Unix: what lands in it later
// is the folder's to guard.
#[test]
fn secure_file_makes_a_folder_private() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("config");
    fs::create_dir(&folder).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = secure_file(dir.path(), &folder);
    assert!(out.status.success(), "{out:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&folder).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}

// doctor tells users to run it, so --help lists it with what it does.
#[test]
fn secure_file_is_listed_in_help() {
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    let line = help
        .lines()
        .find(|line| line.trim_start().starts_with("secure-file"))
        .unwrap_or_else(|| panic!("{help}"));
    assert!(line.contains("readable only by you"), "{line}");
}
