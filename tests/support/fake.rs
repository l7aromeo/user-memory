//! Puts the fake command-line programs of tests/fake/cli.rs on a test's PATH.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Installs the fake as the program `name` in `dir` (`name.exe` on Windows),
/// where its file stem tells it what to be. A hard link when the folders
/// share a file system, else a copy; never a symbolic link, which the fake
/// would resolve to its own name.
pub fn fake(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let built = Path::new(env!("CARGO_BIN_EXE_user-memory-fake"));
    let _ = fs::remove_file(&path);
    if fs::hard_link(built, &path).is_err() {
        fs::copy(built, &path).unwrap();
    }
    path
}

/// Installs the fake as `name` in `dir`, failing at once with `code`: a
/// program that is there but does not work.
pub fn failing(dir: &Path, name: &str, code: u8) -> PathBuf {
    let path = fake(dir, name);
    fs::write(dir.join(format!("{name}.exit")), code.to_string()).unwrap();
    path
}
