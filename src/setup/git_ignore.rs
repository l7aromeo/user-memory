//! Makes git ignore `.user-memory` and `.user-memory-key` for the user:
//! the files configure user-memory on one machine and have no place in a
//! repository's history.

use crate::{
    paths,
    project::{KEY_FILE, LIST_FILE},
};
use anyhow::{Context, Result, bail};
use std::{
    env,
    ffi::OsString,
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    process::{self, Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

/// The files git should ignore, in the order setup adds them.
pub const NAMES: [&str; 2] = [LIST_FILE, KEY_FILE];

/// The line setup writes above the names it adds, once per file.
const MARKER: &str = "# user-memory";

/// Where the user's global ignore stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Ignore {
    /// Git already ignores both files; `target` is the global ignore file.
    Covered(PathBuf),
    /// This run added `names` to `target`.
    Added {
        target: PathBuf,
        names: Vec<&'static str>,
    },
    /// `target` does not ignore `names`, and setup may add them.
    Missing {
        target: PathBuf,
        names: Vec<&'static str>,
    },
    /// The user must add `names` to `target` themselves, for `reason`.
    Manual {
        target: PathBuf,
        names: Vec<&'static str>,
        reason: String,
    },
    /// Git is not installed, so nothing reads an ignore file.
    NoGit,
}

/// `names` as "a" or "a and b".
fn listed(names: &[&str]) -> String {
    names.join(" and ")
}

impl Ignore {
    /// One line for setup and `project-id --set` to print.
    pub fn describe(&self) -> String {
        match self {
            Self::Covered(target) => {
                format!("{} already ignores {}", target.display(), listed(&NAMES))
            }
            Self::Added { target, names } => {
                format!("{} added to {}", listed(names), target.display())
            }
            Self::Missing { target, names } => format!(
                "{} does not ignore {}; run setup to add it",
                target.display(),
                listed(names)
            ),
            Self::Manual {
                target,
                names,
                reason,
            } if target.as_os_str().is_empty() => format!(
                "not written ({reason}); point core.excludesFile at a file and add {} to it",
                listed(names)
            ),
            Self::Manual {
                target,
                names,
                reason,
            } => format!(
                "not written ({reason}); add {} to {} yourself, one per line",
                listed(names),
                target.display()
            ),
            Self::NoGit => "skipped (git not found)".into(),
        }
    }

    /// The step only the user can take, when git does not ignore the files
    /// and setup cannot or did not add them; `None` otherwise.
    pub fn step(&self) -> Option<String> {
        match self {
            Self::Missing { target, names } => Some(format!(
                "{} does not ignore {}; add {} to it, one per line, or run setup again",
                target.display(),
                listed(names),
                listed(names)
            )),
            Self::Manual {
                target,
                names,
                reason,
            } if target.as_os_str().is_empty() => Some(format!(
                "{reason}; point it at a file and add {} to it, one per line",
                listed(names)
            )),
            Self::Manual {
                target,
                names,
                reason,
            } => Some(format!(
                "{reason}; add {} to {} yourself, one per line",
                listed(names),
                target.display()
            )),
            Self::Covered(_) | Self::Added { .. } | Self::NoGit => None,
        }
    }
}

/// Checks the user's global ignore without changing it.
pub fn state() -> Result<Ignore> {
    let Some(target) = target()? else {
        return Ok(Ignore::NoGit);
    };
    let names = uncovered()?;
    if names.is_empty() {
        return Ok(Ignore::Covered(target));
    }
    match unwritable(&target) {
        Some(reason) => Ok(Ignore::Manual {
            target,
            names,
            reason,
        }),
        None => Ok(Ignore::Missing { target, names }),
    }
}

/// Adds each name git does not already ignore to the user's global ignore.
/// Never writes through a symbolic link.
pub fn ensure() -> Result<Ignore> {
    let (target, names) = match state()? {
        Ignore::Missing { target, names } => (target, names),
        other => return Ok(other),
    };
    append(&target, &names)?;
    let left = uncovered()?;
    if !left.is_empty() {
        bail!(
            "added {} to {}, but git still does not ignore {}; check that file",
            listed(&names),
            target.display(),
            listed(&left)
        );
    }
    Ok(Ignore::Added { target, names })
}

/// The user's global ignore file as git finds it: core.excludesFile from
/// the global config (git expands a leading `~`), else
/// `$XDG_CONFIG_HOME/git/ignore`, else `.config/git/ignore` in git's home
/// folder. `None` when git is not installed.
fn target() -> Result<Option<PathBuf>> {
    let Some(out) = git(
        None,
        &[
            "config",
            "--global",
            "--type=path",
            "--get",
            "core.excludesFile",
        ],
    )?
    else {
        return Ok(None);
    };
    // Set, even to nothing: git then reads that file, or none, and never
    // the XDG one.
    if out.status.success() {
        return Ok(Some(PathBuf::from(
            String::from_utf8_lossy(&out.stdout).trim(),
        )));
    }
    // Exit status 1 means the setting is unset.
    if out.status.code() != Some(1) {
        bail!(
            "git config --global --get core.excludesFile failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if let Some(config) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(config).join("git").join("ignore")));
    }
    let home = git_home().context("Cannot locate your home directory")?;
    Ok(Some(home.join(".config").join("git").join("ignore")))
}

/// The home folder git uses for `~` and its global files.
fn git_home() -> Option<PathBuf> {
    git_home_in(cfg!(windows), &|name| env::var_os(name), &|path| {
        path.is_dir()
    })
}

/// [`git_home`] from the environment `env`: `HOME`; on Windows, where it is
/// usually unset, git for Windows then takes `HOMEDRIVE` and `HOMEPATH`
/// when that folder exists, else `USERPROFILE` (its compat/mingw.c).
fn git_home_in(
    windows: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(home) = env("HOME") {
        return Some(home.into());
    }
    if !windows {
        return None;
    }
    if let (Some(drive), Some(path)) = (env("HOMEDRIVE"), env("HOMEPATH")) {
        let mut joined = drive;
        joined.push(path);
        let joined = PathBuf::from(joined);
        if is_dir(&joined) {
            return Some(joined);
        }
    }
    env("USERPROFILE").map(PathBuf::from)
}

/// The lines setup added to the user's global ignore, found by its marker.
pub struct Added {
    pub target: PathBuf,
    /// The marker and every line naming one of [`NAMES`].
    pub lines: Vec<String>,
    /// Why they may not be removed here (a symbolic link), if so.
    pub blocked: Option<String>,
}

/// The lines setup added to the global ignore, or `None` when it has no
/// `# user-memory` marker: lines without it are the user's own.
pub fn added() -> Result<Option<Added>> {
    let Some(target) = target()? else {
        return Ok(None);
    };
    let text = match fs::read_to_string(&target) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", target.display())),
    };
    if !text.lines().any(|line| line.trim() == MARKER) {
        return Ok(None);
    }
    let lines = text
        .lines()
        .filter(|line| ours(line))
        .map(str::to_owned)
        .collect();
    let blocked = unwritable(&target);
    Ok(Some(Added {
        target,
        lines,
        blocked,
    }))
}

/// A line setup writes: its marker or one of [`NAMES`].
fn ours(line: &str) -> bool {
    let line = line.trim();
    line == MARKER || NAMES.contains(&line)
}

/// Removes the lines [`added`] found, keeping every other line as written,
/// and the file when nothing else is left in it.
pub fn remove_added(added: &Added) -> Result<()> {
    if let Some(reason) = &added.blocked {
        bail!("{reason}");
    }
    let text = fs::read_to_string(&added.target)
        .with_context(|| format!("Cannot read {}", added.target.display()))?;
    let kept: String = text
        .split_inclusive('\n')
        .filter(|line| !ours(line))
        .collect();
    if kept.trim().is_empty() {
        fs::remove_file(&added.target)
    } else {
        fs::write(&added.target, kept)
    }
    .with_context(|| format!("Cannot write {}", added.target.display()))
}

/// The files of [`NAMES`] that the repository holding the current folder
/// tracks: an ignore rule never untracks a file.
pub fn tracked_here() -> Result<Vec<PathBuf>> {
    let here = env::current_dir().context("Cannot read the current folder")?;
    let Some(root) = git(Some(&here), &["rev-parse", "--show-toplevel"])?
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    else {
        return Ok(Vec::new());
    };
    let mut args = vec!["ls-files", "--"];
    args.extend(NAMES);
    let Some(out) = git(Some(&root), &args)?.filter(|out| out.status.success()) else {
        return Ok(Vec::new());
    };
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|name| root.join(name))
        .collect())
}

/// Why setup must not write `target`, if it must not: a relative path git
/// would resolve elsewhere, or a symbolic link at the file or at a folder
/// between the home folder and it.
fn unwritable(target: &Path) -> Option<String> {
    if target.as_os_str().is_empty() {
        return Some(
            "core.excludesFile is set to an empty value, so git reads no ignore file".into(),
        );
    }
    if !target.is_absolute() {
        return Some(format!("{} is not an absolute path", target.display()));
    }
    let home = git_home();
    let below_home = |path: &Path| {
        home.as_deref()
            .is_some_and(|home| path != home && path.starts_with(home))
    };
    let link = target
        .ancestors()
        .filter(|path| *path == target || below_home(path))
        .find(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()))?;
    Some(format!(
        "{} is a symbolic link; user-memory leaves it and its target as they are",
        link.display()
    ))
}

/// Appends `names` to `target`, creating it and its folder if missing,
/// after a newline when the file does not end with one, and under the
/// marker unless the file already has it.
fn append(target: &Path, names: &[&str]) -> Result<()> {
    if let Some(folder) = target.parent() {
        fs::create_dir_all(folder)
            .with_context(|| format!("Cannot create {}", folder.display()))?;
    }
    let existing = match fs::read(target) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", target.display())),
    };
    let mut block = String::new();
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        block.push('\n');
    }
    if !String::from_utf8_lossy(&existing)
        .lines()
        .any(|line| line.trim() == MARKER)
    {
        block.push_str(MARKER);
        block.push('\n');
    }
    for name in names {
        block.push_str(name);
        block.push('\n');
    }
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(target)
        .and_then(|mut file| file.write_all(block.as_bytes()))
        .with_context(|| format!("Cannot write {}", target.display()))
}

/// The names of [`NAMES`] git, as the user configures it, does not ignore
/// at a repository's root, asked of a fresh empty repository so no
/// repository's own rules count.
fn uncovered() -> Result<Vec<&'static str>> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let probe = env::temp_dir().join(format!("user-memory-ignore-{}-{nanos}", process::id()));
    fs::create_dir(&probe).with_context(|| format!("Cannot create {}", probe.display()))?;
    let answer = (|| {
        let init =
            git(Some(&probe), &["init", "-q", "--template="])?.context("git is no longer found")?;
        if !init.status.success() {
            bail!(
                "git init failed in {}: {}",
                probe.display(),
                String::from_utf8_lossy(&init.stderr).trim()
            );
        }
        let mut args = vec!["check-ignore", "--"];
        args.extend(NAMES);
        let check = git(Some(&probe), &args)?.context("git is no longer found")?;
        // 0: some are ignored, listed on stdout; 1: none is.
        if !matches!(check.status.code(), Some(0 | 1)) {
            bail!(
                "git check-ignore failed: {}",
                String::from_utf8_lossy(&check.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&check.stdout);
        let ignored: Vec<&str> = stdout.lines().map(str::trim).collect();
        Ok(NAMES
            .into_iter()
            .filter(|name| !ignored.contains(name))
            .collect())
    })();
    let _ = fs::remove_dir_all(&probe);
    answer
}

/// Where a lookup that needs no repository runs: git reads the current
/// folder's repository even for `config --global`, and fails when that
/// checkout is broken. The file-system root is not a checkout on any
/// ordinary system: `/`, or on Windows the root of the drive user-memory is
/// installed on.
fn neutral_dir() -> PathBuf {
    paths::bin_dir()
        .and_then(|bin| bin.ancestors().last().map(Path::to_path_buf))
        .filter(|root| !root.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Runs git (never through a shell), in `dir`, or in [`neutral_dir`] for a
/// lookup that needs no repository, free of any repository the environment
/// points at. `None` when git is not installed.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<Option<Output>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir.map_or_else(neutral_dir, Path::to_path_buf))
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    match command.output() {
        Ok(out) => Ok(Some(out)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("Cannot run git"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git for Windows' own order, checked with a made-up environment on
    /// every platform.
    #[test]
    fn git_finds_its_home_as_git_for_windows_does() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        const PROFILE: &[(&str, &str)] = &[("USERPROFILE", r"C:\Users\dev")];
        const DRIVE: &[(&str, &str)] = &[
            ("HOMEDRIVE", "D:"),
            ("HOMEPATH", r"\home\dev"),
            ("USERPROFILE", r"C:\Users\dev"),
        ];
        const HOME: &[(&str, &str)] = &[("HOME", "/home/dev"), ("USERPROFILE", r"C:\Users\dev")];
        let exists = |_: &Path| true;
        let missing = |_: &Path| false;
        assert_eq!(
            git_home_in(true, &env(HOME), &exists),
            Some(PathBuf::from("/home/dev"))
        );
        assert_eq!(
            git_home_in(true, &env(DRIVE), &exists),
            Some(PathBuf::from(r"D:\home\dev"))
        );
        assert_eq!(
            git_home_in(true, &env(DRIVE), &missing),
            Some(PathBuf::from(r"C:\Users\dev"))
        );
        assert_eq!(
            git_home_in(true, &env(PROFILE), &exists),
            Some(PathBuf::from(r"C:\Users\dev"))
        );
        // Unix git reads HOME only.
        assert_eq!(
            git_home_in(false, &env(HOME), &exists),
            Some(PathBuf::from("/home/dev"))
        );
        assert_eq!(git_home_in(false, &env(PROFILE), &exists), None);
    }
}
