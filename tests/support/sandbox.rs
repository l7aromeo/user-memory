//! The environment that keeps what a test runs inside its own folder, on
//! every platform: user-memory, git and the clients find their homes, data,
//! state and configuration there, never in the profile of the machine
//! running the tests, where a test would touch the user's real store,
//! rules or PATH.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

/// Variables that point a command at a store or server of the machine's;
/// a sandboxed command has none unless its test sets one.
const ELSEWHERE: &[&str] = &[
    "USER_MEMORY_DB",
    "USER_MEMORY_URL",
    "USER_MEMORY_TOKEN_FILE",
    "USER_MEMORY_DOCKER_NAME",
];

pub trait Sandboxed {
    /// Puts the command's whole environment for finding files into `root`:
    /// - HOME and the XDG folders (Unix);
    /// - USERPROFILE, LOCALAPPDATA and APPDATA (Windows);
    /// - Claude Code's and Codex's configuration;
    /// - git's global configuration (in `root`, where none exists until a
    ///   test writes one, which git reads as empty) and no system one;
    /// - no update check, and no client apps found outside `root`.
    ///
    /// Each points where the program would derive it from `root`, so a
    /// test sees the program's own defaults. A variable the test has
    /// already set or removed on the command is kept as the test made it,
    /// whatever the order of the calls.
    fn sandboxed(&mut self, root: impl AsRef<Path>) -> &mut Self;
}

impl Sandboxed for Command {
    fn sandboxed(&mut self, root: impl AsRef<Path>) -> &mut Self {
        let root = root.as_ref();
        let given: HashSet<OsString> = self.get_envs().map(|(key, _)| key.to_owned()).collect();
        let settings: [(&str, OsString); 13] = [
            ("HOME", root.into()),
            ("XDG_DATA_HOME", root.join(".local").join("share").into()),
            ("XDG_STATE_HOME", root.join(".local").join("state").into()),
            ("XDG_CONFIG_HOME", root.join(".config").into()),
            ("USERPROFILE", root.into()),
            ("LOCALAPPDATA", root.join("AppData").join("Local").into()),
            ("APPDATA", root.join("AppData").join("Roaming").into()),
            ("CLAUDE_CONFIG_DIR", root.join(".claude").into()),
            ("CODEX_HOME", root.join(".codex").into()),
            ("GIT_CONFIG_GLOBAL", root.join(".gitconfig").into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("USER_MEMORY_NO_UPDATE_CHECK", "1".into()),
            ("USER_MEMORY_APP_DIRS", "".into()),
        ];
        for (key, value) in settings {
            if !given.contains(OsStr::new(key)) {
                self.env(key, value);
            }
        }
        for key in ELSEWHERE {
            if !given.contains(OsStr::new(key)) {
                self.env_remove(key);
            }
        }
        self
    }
}

/// Where user-memory, run in `root`'s sandbox, keeps its folder `name`:
/// `home`, `data`, `state`, `config`, `bin`, `docker` or `store`. The debug
/// build's own answer (`debug-paths`), so no test spells out a platform's
/// layout itself.
pub fn folder(root: &Path, name: &str) -> PathBuf {
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("debug-paths")
        .sandboxed(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let folders: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    PathBuf::from(
        folders[name]
            .as_str()
            .unwrap_or_else(|| panic!("no {name} in {folders}")),
    )
}
