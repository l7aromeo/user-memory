//! Where user-memory keeps its files on this machine, and when two paths
//! name one file.
//!
//! macOS and Linux follow the XDG base directories under the home folder;
//! Windows keeps everything under `%LOCALAPPDATA%`, never the roaming
//! `%APPDATA%`. The rules are pure functions of the environment, so both
//! platforms' rules are tested on every platform.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The binary's file name in the bin folder.
pub const BINARY: &str = if cfg!(windows) {
    "user-memory.exe"
} else {
    "user-memory"
};

/// The Docker project name install.sh uses unless told another, whose
/// folder carries no suffix.
const DOCKER_NAME: &str = "user-memory";

/// Whose conventions a path follows; a value, so tests check both.
#[derive(Clone, Copy)]
enum Os {
    Unix,
    Windows,
}

const OS: Os = if cfg!(windows) { Os::Windows } else { Os::Unix };

/// The home folder: `HOME` as set on Unix, the profile folder on Windows,
/// where `HOME` is usually unset.
pub fn home() -> Option<PathBuf> {
    if cfg!(windows) {
        env::home_dir()
    } else {
        env::var_os("HOME").map(PathBuf::from)
    }
}

/// The data folder: `$XDG_DATA_HOME/user-memory`, else
/// `~/.local/share/user-memory`; `%LOCALAPPDATA%\user-memory` on Windows.
pub fn data_dir() -> Option<PathBuf> {
    data_in(OS, &variable, home().as_deref())
}

/// The state folder (session snapshots): `$XDG_STATE_HOME/user-memory`, else
/// `~/.local/state/user-memory`; `%LOCALAPPDATA%\user-memory\state` on
/// Windows.
pub fn state_dir() -> Option<PathBuf> {
    state_in(OS, &variable, home().as_deref())
}

/// The config folder (the client token): `$XDG_CONFIG_HOME/user-memory`,
/// else `~/.config/user-memory`; `%LOCALAPPDATA%\user-memory\config` on
/// Windows.
pub fn config_dir() -> Option<PathBuf> {
    config_in(OS, &variable, home().as_deref())
}

/// The bin folder: `~/.local/bin`; `%LOCALAPPDATA%\Programs\user-memory\bin`
/// on Windows.
pub fn bin_dir() -> Option<PathBuf> {
    bin_in(OS, &variable, home().as_deref())
}

/// The folder of the installer's Docker server for Docker project `name`:
/// `docker` in the data folder, `docker-<name>` for a name of the user's.
pub fn docker_dir(name: &str) -> Option<PathBuf> {
    data_dir().map(|data| docker_in(&data, name))
}

/// The default store. On Unix it is `~/.local/share/user-memory`, whatever
/// `XDG_DATA_HOME` says: existing stores live there, and following the
/// variable would leave them behind.
pub fn store() -> PathBuf {
    store_in(OS, &variable, home().as_deref())
}

/// Whether `a` and `b` name one file or folder: compared once resolved
/// where they exist, as given where they do not; on Windows, whose file
/// systems ignore case, ignoring ASCII case.
pub fn same(a: &Path, b: &Path) -> bool {
    let resolved = |path: &Path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    equal(OS, &resolved(a), &resolved(b))
}

fn variable(name: &str) -> Option<OsString> {
    env::var_os(name)
}

/// An environment the rules read: a variable's value by name.
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// `$name` when it is set to an absolute path, else `default` under a
/// non-empty `home`: a relative value is ignored, as the XDG base directory
/// specification says. Absolute by Unix's rule, a leading `/`, whatever the
/// platform running the rule, so its tests hold on Windows too.
fn xdg(env: Env, name: &str, default: &str, home: Option<&Path>) -> Option<PathBuf> {
    env(name)
        .map(PathBuf::from)
        .filter(|dir| dir.to_string_lossy().starts_with('/'))
        .or_else(|| {
            home.filter(|home| !home.as_os_str().is_empty())
                .map(|home| home.join(default))
        })
}

/// `%LOCALAPPDATA%`, else `AppData\Local` under `home`.
fn local_app_data(env: Env, home: Option<&Path>) -> Option<PathBuf> {
    env("LOCALAPPDATA")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join("AppData").join("Local")))
}

fn data_in(os: Os, env: Env, home: Option<&Path>) -> Option<PathBuf> {
    let base = match os {
        Os::Unix => xdg(env, "XDG_DATA_HOME", ".local/share", home),
        Os::Windows => local_app_data(env, home),
    };
    base.map(|base| base.join("user-memory"))
}

fn state_in(os: Os, env: Env, home: Option<&Path>) -> Option<PathBuf> {
    match os {
        Os::Unix => {
            xdg(env, "XDG_STATE_HOME", ".local/state", home).map(|base| base.join("user-memory"))
        }
        Os::Windows => data_in(os, env, home).map(|data| data.join("state")),
    }
}

fn config_in(os: Os, env: Env, home: Option<&Path>) -> Option<PathBuf> {
    match os {
        Os::Unix => {
            xdg(env, "XDG_CONFIG_HOME", ".config", home).map(|base| base.join("user-memory"))
        }
        Os::Windows => data_in(os, env, home).map(|data| data.join("config")),
    }
}

fn bin_in(os: Os, env: Env, home: Option<&Path>) -> Option<PathBuf> {
    match os {
        Os::Unix => home
            .filter(|home| !home.as_os_str().is_empty())
            .map(|home| home.join(".local/bin")),
        Os::Windows => local_app_data(env, home)
            .map(|local| local.join("Programs").join("user-memory").join("bin")),
    }
}

fn docker_in(data: &Path, name: &str) -> PathBuf {
    if name == DOCKER_NAME {
        data.join("docker")
    } else {
        data.join(format!("docker-{name}"))
    }
}

/// The store under `HOME`, else `USERPROFILE`, else the current folder on
/// Unix; under the data folder, else the current folder, on Windows.
fn store_in(os: Os, env: Env, home: Option<&Path>) -> PathBuf {
    match os {
        Os::Unix => PathBuf::from(
            env("HOME")
                .or_else(|| env("USERPROFILE"))
                .unwrap_or_else(|| ".".into()),
        )
        .join(".local/share/user-memory/memories.sqlite3"),
        Os::Windows => data_in(os, env, home)
            .unwrap_or_else(|| PathBuf::from("user-memory"))
            .join("memories.sqlite3"),
    }
}

/// Whether two resolved paths are one, component by component: exactly on
/// Unix, ignoring ASCII case on Windows.
fn equal(os: Os, a: &Path, b: &Path) -> bool {
    match os {
        Os::Unix => a == b,
        Os::Windows => {
            a.components().count() == b.components().count()
                && a.components()
                    .zip(b.components())
                    .all(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An environment holding exactly `vars`.
    fn with(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| vars.get(name).cloned()
    }

    fn path(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn unix_folders_follow_xdg_and_ignore_relative_or_empty_values() {
        let home = Some(Path::new("/home/u"));
        let none = with(&[]);
        assert_eq!(
            data_in(Os::Unix, &none, home),
            Some(path(&["/home/u/.local/share", "user-memory"]))
        );
        assert_eq!(
            state_in(Os::Unix, &none, home),
            Some(path(&["/home/u/.local/state", "user-memory"]))
        );
        assert_eq!(
            config_in(Os::Unix, &none, home),
            Some(path(&["/home/u/.config", "user-memory"]))
        );
        assert_eq!(
            bin_in(Os::Unix, &none, home),
            Some(path(&["/home/u/.local/bin"]))
        );
        let set = with(&[
            ("XDG_DATA_HOME", "/d"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_CONFIG_HOME", "/c"),
            ("LOCALAPPDATA", "/l"),
        ]);
        assert_eq!(
            data_in(Os::Unix, &set, home),
            Some(path(&["/d", "user-memory"]))
        );
        assert_eq!(
            state_in(Os::Unix, &set, home),
            Some(path(&["/s", "user-memory"]))
        );
        assert_eq!(
            config_in(Os::Unix, &set, home),
            Some(path(&["/c", "user-memory"]))
        );
        assert_eq!(
            bin_in(Os::Unix, &set, home),
            Some(path(&["/home/u/.local/bin"]))
        );
        for value in ["relative", ""] {
            let ignored = with(&[("XDG_STATE_HOME", value)]);
            assert_eq!(
                state_in(Os::Unix, &ignored, home),
                Some(path(&["/home/u/.local/state", "user-memory"])),
                "{value:?}"
            );
        }
        for home in [None, Some(Path::new(""))] {
            assert_eq!(data_in(Os::Unix, &none, home), None, "{home:?}");
            assert_eq!(state_in(Os::Unix, &none, home), None, "{home:?}");
            assert_eq!(config_in(Os::Unix, &none, home), None, "{home:?}");
            assert_eq!(bin_in(Os::Unix, &none, home), None, "{home:?}");
        }
    }

    #[test]
    fn windows_folders_live_under_local_app_data() {
        let home = Some(Path::new("C:/Users/u"));
        let local = with(&[("LOCALAPPDATA", "D:/Local"), ("XDG_DATA_HOME", "/d")]);
        assert_eq!(
            data_in(Os::Windows, &local, home),
            Some(path(&["D:/Local", "user-memory"]))
        );
        assert_eq!(
            state_in(Os::Windows, &local, home),
            Some(path(&["D:/Local", "user-memory", "state"]))
        );
        assert_eq!(
            config_in(Os::Windows, &local, home),
            Some(path(&["D:/Local", "user-memory", "config"]))
        );
        assert_eq!(
            bin_in(Os::Windows, &local, home),
            Some(path(&["D:/Local", "Programs", "user-memory", "bin"]))
        );
        assert_eq!(
            store_in(Os::Windows, &local, home),
            path(&["D:/Local", "user-memory", "memories.sqlite3"])
        );
        // Without LOCALAPPDATA, the profile's AppData\Local.
        for unset in [with(&[]), with(&[("LOCALAPPDATA", "")])] {
            assert_eq!(
                data_in(Os::Windows, &unset, home),
                Some(path(&["C:/Users/u", "AppData", "Local", "user-memory"]))
            );
        }
        assert_eq!(data_in(Os::Windows, &with(&[]), None), None);
        assert_eq!(
            store_in(Os::Windows, &with(&[]), None),
            path(&["user-memory", "memories.sqlite3"])
        );
    }

    #[test]
    fn the_unix_store_stays_where_existing_stores_are() {
        let store = |vars: &[(&str, &str)]| store_in(Os::Unix, &with(vars), None);
        let under = |home: &str| path(&[home, ".local/share/user-memory/memories.sqlite3"]);
        assert_eq!(
            store(&[("HOME", "/h"), ("XDG_DATA_HOME", "/d")]),
            under("/h")
        );
        assert_eq!(store(&[("HOME", "/h"), ("USERPROFILE", "/p")]), under("/h"));
        assert_eq!(store(&[("USERPROFILE", "/p")]), under("/p"));
        assert_eq!(store(&[]), under("."));
    }

    #[test]
    fn the_docker_folder_names_a_project_of_the_users() {
        let data = Path::new("/d/user-memory");
        assert_eq!(docker_in(data, "user-memory"), data.join("docker"));
        assert_eq!(docker_in(data, "work"), data.join("docker-work"));
    }

    #[test]
    fn paths_are_equal_by_bytes_on_unix_and_ignoring_case_on_windows() {
        let (a, b) = (Path::new("C:/Users/U/bin"), Path::new("c:/users/u/BIN"));
        assert!(equal(Os::Windows, a, b));
        assert!(!equal(Os::Unix, a, b));
        assert!(equal(Os::Unix, a, a));
        assert!(!equal(Os::Windows, a, Path::new("C:/Users/U/bin2")));
        assert!(!equal(Os::Windows, a, Path::new("C:/Users/U/bin/x")));
    }

    #[test]
    fn same_resolves_existing_paths_and_compares_others_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(same(&folder, &folder.join("..").join("folder")));
        assert!(!same(&folder, dir.path()));
        let missing = dir.path().join("missing");
        assert!(same(&missing, &missing));
        assert!(!same(&missing, &dir.path().join("..").join("missing")));
    }
}
