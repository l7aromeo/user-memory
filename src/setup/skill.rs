//! The user-memory Agent Skill: playbooks for rare, user-initiated workflows,
//! installed where the user's skills already live.
//!
//! The real folder is `~/.agents/skills/user-memory` (read by Codex and other
//! agents); Claude Code gets a relative symlink to it from
//! `$CLAUDE_CONFIG_DIR/skills/user-memory`, like the user's skills manager
//! lays out its own skills. Setup owns only what carries its marker file: a
//! folder or link it did not create is refused and left alone, and the skills
//! manager's lock file is never read or written.

use crate::paths;
use anyhow::{Context, Result};
use std::{
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

const NAME: &str = "user-memory";
/// Marks a folder setup created; holds the version of the files inside.
const MARKER: &str = ".user-memory-managed";

/// The skill's files as (relative name, contents).
pub(super) type Files = &'static [(&'static str, &'static str)];

/// The skill as built into this binary, from `skills/user-memory/`.
pub(super) const BUNDLED: Files = &[
    (
        "SKILL.md",
        include_str!("../../skills/user-memory/SKILL.md"),
    ),
    (
        "backup.md",
        include_str!("../../skills/user-memory/backup.md"),
    ),
    (
        "forget.md",
        include_str!("../../skills/user-memory/forget.md"),
    ),
    (
        "projects.md",
        include_str!("../../skills/user-memory/projects.md"),
    ),
    (
        "review.md",
        include_str!("../../skills/user-memory/review.md"),
    ),
    (
        "upgrade.md",
        include_str!("../../skills/user-memory/upgrade.md"),
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Added,
    Updated,
    Removed,
    Unchanged,
}

impl Change {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Removed => "removed",
            Self::Unchanged => "unchanged",
        }
    }
}

/// What doctor finds.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum State {
    Ok,
    Outdated,
    Missing,
    /// Someone else installed a skill with this name; left alone.
    Foreign(PathBuf),
}

/// Where the skill goes: the shared skills folder and, for Claude Code, the
/// folder that links to it.
pub(super) struct Places {
    pub(super) agents: PathBuf,
    pub(super) claude: Option<PathBuf>,
}

impl Places {
    /// `~/.agents/skills`, plus `$CLAUDE_CONFIG_DIR/skills` (default
    /// `~/.claude/skills`) when Claude Code is one of setup's clients.
    pub(super) fn for_clients(claude: bool) -> Result<Self> {
        let home = paths::home().context("Cannot locate your home directory")?;
        let claude =
            claude.then(
                || match env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
                    Some(dir) => PathBuf::from(dir).join("skills"),
                    None => home.join(".claude").join("skills"),
                },
            );
        Ok(Self {
            agents: home.join(".agents").join("skills"),
            claude,
        })
    }
    /// Where the skill is, for setup's output.
    pub(super) fn describe(&self) -> String {
        match self.link() {
            Some(link) => format!(
                "{}, linked from {}",
                self.folder().display(),
                link.display()
            ),
            None => self.folder().display().to_string(),
        }
    }
    fn folder(&self) -> PathBuf {
        self.agents.join(NAME)
    }
    fn link(&self) -> Option<PathBuf> {
        self.claude.as_ref().map(|dir| dir.join(NAME))
    }
}

/// Version of a set of files: FNV-1a over names and contents, stable across builds.
fn version(files: Files) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, contents) in files {
        for byte in name.bytes().chain([0]).chain(contents.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

fn marker(files: Files) -> String {
    format!("user-memory skill {}\n", version(files))
}

/// How setup relates to whatever sits at `path`.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Nothing,
    /// A folder with our marker; `current` when it holds exactly `files`.
    Ours {
        current: bool,
    },
    /// A symlink pointing at our folder.
    OurLink,
    Foreign,
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

fn inspect_folder(path: &Path, files: Files) -> Result<Found> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Found::Nothing),
        Err(err) => return Err(err).with_context(|| format!("Cannot inspect {}", path.display())),
    };
    if !metadata.is_dir() || !path.join(MARKER).is_file() {
        return Ok(Found::Foreign);
    }
    let mut expected: Vec<(String, String)> = files
        .iter()
        .map(|(name, contents)| ((*name).to_owned(), (*contents).to_owned()))
        .collect();
    expected.push((MARKER.to_owned(), marker(files)));
    expected.sort();
    let mut present = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let contents = fs::read_to_string(entry.path()).unwrap_or_default();
        present.push((name, contents));
    }
    present.sort();
    Ok(Found::Ours {
        current: present == expected,
    })
}

fn inspect_link(link: &Path, folder: &Path, files: Files) -> Result<Found> {
    if !is_link(link) {
        // Where symlinks are unavailable setup leaves a marked copy instead.
        return inspect_folder(link, files);
    }
    // Compared by resolved paths, or, while our folder is missing, by what
    // the link says, so our own dangling link is never taken for another's.
    let points_at_ours = (link.exists() && folder.exists() && paths::same(link, folder))
        || fs::read_link(link).is_ok_and(|target| {
            let parent = link.parent().unwrap_or(Path::new(""));
            lexical(&parent.join(target)) == lexical(folder)
        });
    Ok(if points_at_ours {
        Found::OurLink
    } else {
        Found::Foreign
    })
}

/// `path` with `.` and `..` resolved by name, not through the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The path from `from` (a directory) to `to`, both absolute, using `..`.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path: PathBuf =
        std::iter::repeat_n(Component::ParentDir, from.len() - shared).collect();
    path.extend(&to[shared..]);
    path
}

/// Writes `files` and the marker into a new folder at `path`, atomically:
/// a temporary sibling is filled, then renamed into place.
fn write_folder(path: &Path, files: Files) -> Result<()> {
    let parent = path.parent().context("Invalid skill path")?;
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    let staging = parent.join(format!(".{NAME}.setup-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let result = (|| -> Result<()> {
        fs::create_dir(&staging)?;
        for (name, contents) in files {
            fs::write(staging.join(name), contents)?;
        }
        // Closed before the folder is renamed: Windows renames no folder
        // with a file open inside it.
        {
            let mut stamp = fs::File::create(staging.join(MARKER))?;
            stamp.write_all(marker(files).as_bytes())?;
            stamp.sync_all()?;
        }
        if fs::symlink_metadata(path).is_ok() {
            // Only ever our own folder: callers check before replacing.
            let old = parent.join(format!(".{NAME}.old-{}", std::process::id()));
            fs::rename(path, &old)?;
            fs::rename(&staging, path)?;
            fs::remove_dir_all(&old)?;
        } else {
            fs::rename(&staging, path)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result.with_context(|| format!("Cannot install the skill in {}", path.display()))
}

fn foreign(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{} was not installed by user-memory setup (for example by a skills manager) and was left alone; remove it, or run setup with --no-skill",
        path.display()
    )
}

/// Installs or updates the skill; nothing is written when it is current.
pub(super) fn install(places: &Places, files: Files) -> Result<Change> {
    let folder = places.folder();
    let found = inspect_folder(&folder, files)?;
    if found == Found::Foreign {
        return Err(foreign(&folder));
    }
    let link = places.link();
    let link_found = match &link {
        Some(link) => Some(inspect_link(link, &folder, files)?),
        None => None,
    };
    if link_found == Some(Found::Foreign) {
        return Err(foreign(link.as_ref().expect("checked")));
    }
    let mut change = match found {
        Found::Nothing => {
            write_folder(&folder, files)?;
            Change::Added
        }
        Found::Ours { current: false } => {
            write_folder(&folder, files)?;
            Change::Updated
        }
        _ => Change::Unchanged,
    };
    if let (Some(link), Some(link_found)) = (link, link_found) {
        let linked = match link_found {
            Found::Nothing => {
                let parent = link.parent().context("Invalid skill path")?;
                fs::create_dir_all(parent)
                    .with_context(|| format!("Cannot create {}", parent.display()))?;
                if symlink(&relative(parent, &folder), &link).is_err() {
                    write_folder(&link, files)?;
                }
                true
            }
            Found::Ours { current: false } => {
                write_folder(&link, files)?;
                true
            }
            _ => false,
        };
        if linked && change == Change::Unchanged {
            change = Change::Updated;
        }
    }
    Ok(change)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// A directory symlink, which Windows creates only with Developer Mode on
/// or as an administrator; otherwise the marked copy stands in.
#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(not(any(unix, windows)))]
fn symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::other("symlinks are not available"))
}

/// Deletes the link `path`, never what it points to. Windows deletes a
/// directory link as a folder, not as a file.
fn remove_link(path: &Path) -> io::Result<()> {
    fs::remove_file(path).or_else(|_| fs::remove_dir(path))
}

/// Removes setup's own folder and link; anything else is left alone.
pub(super) fn remove(places: &Places, files: Files) -> Result<Change> {
    let folder = places.folder();
    let mut change = Change::Unchanged;
    if let Some(link) = places.link() {
        match inspect_link(&link, &folder, files)? {
            Found::OurLink => {
                remove_link(&link)?;
                change = Change::Removed;
            }
            Found::Ours { .. } => {
                fs::remove_dir_all(&link)?;
                change = Change::Removed;
            }
            Found::Nothing | Found::Foreign => {}
        }
    }
    if let Found::Ours { .. } = inspect_folder(&folder, files)? {
        fs::remove_dir_all(&folder)?;
        change = Change::Removed;
    }
    Ok(change)
}

/// The skill's state for doctor.
pub(super) fn state(places: &Places, files: Files) -> Result<State> {
    let folder = places.folder();
    let found = inspect_folder(&folder, files)?;
    if found == Found::Foreign {
        return Ok(State::Foreign(folder));
    }
    // Someone else's link counts even where our folder is missing: setup
    // would leave it alone and install nothing.
    if let Some(link) = places.link()
        && inspect_link(&link, &folder, files)? == Found::Foreign
    {
        return Ok(State::Foreign(link));
    }
    match found {
        Found::Nothing => return Ok(State::Missing),
        Found::Ours { current: false } => return Ok(State::Outdated),
        _ => {}
    }
    if let Some(link) = places.link() {
        match inspect_link(&link, &folder, files)? {
            Found::Nothing => return Ok(State::Missing),
            Found::Foreign => return Ok(State::Foreign(link)),
            Found::Ours { current: false } => return Ok(State::Outdated),
            _ => {}
        }
    }
    Ok(State::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const FILES: Files = &[
        (
            "SKILL.md",
            "---\nname: user-memory\ndescription: test\n---\n# Skill\n",
        ),
        ("review.md", "# Review\n"),
    ];
    const NEWER: Files = &[
        (
            "SKILL.md",
            "---\nname: user-memory\ndescription: test\n---\n# Skill v2\n",
        ),
        ("review.md", "# Review\n"),
    ];

    /// Claude Code's entry for the skill: the relative link, or, where
    /// Windows makes no link (without Developer Mode or an administrator),
    /// the marked copy standing in for it.
    fn assert_linked(link: &Path, files: Files) {
        if is_link(link) || cfg!(unix) {
            assert_eq!(
                fs::read_link(link).unwrap(),
                Path::new("../../.agents/skills/user-memory")
            );
        } else {
            assert_eq!(
                fs::read_to_string(link.join(MARKER)).unwrap(),
                marker(files),
                "neither our link nor our copy at {}",
                link.display()
            );
        }
    }

    /// Deletes Claude Code's entry, link or copy, as a user might.
    fn delete_entry(link: &Path) {
        if is_link(link) {
            remove_link(link).unwrap();
        } else {
            fs::remove_dir_all(link).unwrap();
        }
    }

    fn places(root: &Path, claude: bool) -> Places {
        Places {
            agents: root.join("home/.agents/skills"),
            claude: claude.then(|| root.join("home/.claude/skills")),
        }
    }

    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path.clone());
                }
                let bytes = if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                found.push((path, bytes, meta.modified().unwrap()));
            }
        }
        found.sort();
        found
    }

    #[test]
    fn installs_a_real_folder_and_a_relative_claude_link_once() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), true);
        assert_eq!(install(&p, FILES).unwrap(), Change::Added);
        let folder = p.agents.join(NAME);
        assert_eq!(
            fs::read_to_string(folder.join("review.md")).unwrap(),
            "# Review\n"
        );
        assert_eq!(
            fs::read_to_string(folder.join(MARKER)).unwrap(),
            marker(FILES)
        );
        let link = p.claude.as_ref().unwrap().join(NAME);
        assert_linked(&link, FILES);
        assert_eq!(state(&p, FILES).unwrap(), State::Ok);
        let before = snapshot(tmp.path());
        assert_eq!(install(&p, FILES).unwrap(), Change::Unchanged);
        assert_eq!(snapshot(tmp.path()), before, "a re-run rewrites nothing");
        assert_eq!(install(&p, NEWER).unwrap(), Change::Updated);
        assert_eq!(
            fs::read_to_string(folder.join("SKILL.md")).unwrap(),
            NEWER[0].1
        );
        assert_eq!(state(&p, NEWER).unwrap(), State::Ok);
        assert_eq!(state(&p, FILES).unwrap(), State::Outdated);
        let leftovers: Vec<_> = fs::read_dir(&p.agents)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [NAME], "no staging folders left behind");
    }

    #[test]
    fn without_claude_only_the_shared_folder_is_installed() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), false);
        assert_eq!(install(&p, FILES).unwrap(), Change::Added);
        assert!(!tmp.path().join("home/.claude").exists());
        assert_eq!(state(&p, FILES).unwrap(), State::Ok);
    }

    #[test]
    fn a_missing_link_or_tampered_file_is_reported_and_repaired() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), true);
        install(&p, FILES).unwrap();
        let link = p.claude.as_ref().unwrap().join(NAME);
        delete_entry(&link);
        assert_eq!(state(&p, FILES).unwrap(), State::Missing);
        assert_eq!(install(&p, FILES).unwrap(), Change::Updated);
        fs::write(p.agents.join(NAME).join("review.md"), "edited").unwrap();
        assert_eq!(state(&p, FILES).unwrap(), State::Outdated);
        fs::write(p.agents.join(NAME).join("extra.md"), "x").unwrap();
        assert_eq!(install(&p, FILES).unwrap(), Change::Updated);
        assert!(!p.agents.join(NAME).join("extra.md").exists());
        assert_eq!(state(&p, FILES).unwrap(), State::Ok);
        fs::remove_dir_all(p.agents.join(NAME)).unwrap();
        assert_eq!(state(&p, FILES).unwrap(), State::Missing);
    }

    #[test]
    fn a_foreign_folder_is_refused_and_left_alone() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), true);
        let folder = p.agents.join(NAME);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("SKILL.md"), "someone else's").unwrap();
        let before = snapshot(tmp.path());
        let err = install(&p, FILES).unwrap_err().to_string();
        assert!(
            err.contains("was not installed by user-memory setup"),
            "{err}"
        );
        assert_eq!(state(&p, FILES).unwrap(), State::Foreign(folder.clone()));
        assert_eq!(remove(&p, FILES).unwrap(), Change::Unchanged);
        assert_eq!(snapshot(tmp.path()), before);
    }

    /// Someone else's link in Claude Code's skills folder. Windows makes a
    /// link only with Developer Mode or as an administrator, so this runs
    /// on macOS and Linux.
    #[cfg(unix)]
    #[test]
    fn a_foreign_link_is_refused_and_left_alone() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), true);
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let link = p.claude.as_ref().unwrap().join(NAME);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(install(&p, FILES).is_err());
        assert!(
            !p.agents.join(NAME).exists(),
            "nothing written before refusing"
        );
        // Someone else's link counts whether or not our folder is there.
        assert_eq!(state(&p, FILES).unwrap(), State::Foreign(link.clone()));
        install(&places(tmp.path(), false), FILES).unwrap();
        assert_eq!(state(&p, FILES).unwrap(), State::Foreign(link.clone()));
        assert_eq!(remove(&p, FILES).unwrap(), Change::Removed);
        assert_eq!(
            fs::read_link(&link).unwrap(),
            elsewhere,
            "foreign link kept"
        );
    }

    #[test]
    fn opting_out_removes_only_our_folder_and_link() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), true);
        assert_eq!(remove(&p, FILES).unwrap(), Change::Unchanged);
        install(&p, FILES).unwrap();
        let other = p.agents.join("other-skill");
        fs::create_dir_all(&other).unwrap();
        assert_eq!(remove(&p, FILES).unwrap(), Change::Removed);
        assert!(!p.agents.join(NAME).exists());
        assert!(fs::symlink_metadata(p.claude.as_ref().unwrap().join(NAME)).is_err());
        assert!(other.exists());
        assert_eq!(remove(&p, FILES).unwrap(), Change::Unchanged);
    }

    #[test]
    fn a_marked_copy_stands_in_for_the_link_where_symlinks_fail() {
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), true);
        install(&places(tmp.path(), false), FILES).unwrap();
        let copy = p.claude.as_ref().unwrap().join(NAME);
        write_folder(&copy, FILES).unwrap();
        assert_eq!(state(&p, FILES).unwrap(), State::Ok);
        install(&places(tmp.path(), false), NEWER).unwrap();
        assert_eq!(
            state(&p, NEWER).unwrap(),
            State::Outdated,
            "the copy is behind"
        );
        assert_eq!(install(&p, NEWER).unwrap(), Change::Updated);
        assert_eq!(
            fs::read_to_string(copy.join("SKILL.md")).unwrap(),
            NEWER[0].1
        );
        assert_eq!(remove(&p, NEWER).unwrap(), Change::Removed);
        assert!(!copy.exists());
    }

    #[test]
    fn the_marker_names_a_version_that_is_stable_across_builds() {
        // FNV-1a over "a\0\0": the version must not depend on the compiler.
        assert_eq!(marker(&[("a", "")]), "user-memory skill e5d2f7190427066c\n");
        assert_eq!(version(&[]), "cbf29ce484222325");
    }

    /// Permission bits make the folder unreadable, which exist only on Unix.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_skills_folder_is_an_error_not_a_missing_skill() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let p = places(tmp.path(), false);
        fs::create_dir_all(&p.agents).unwrap();
        fs::set_permissions(&p.agents, fs::Permissions::from_mode(0o000)).unwrap();
        let checked = state(&p, FILES);
        let installed = install(&p, FILES);
        fs::set_permissions(&p.agents, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(checked.is_err(), "{checked:?}");
        let err = format!("{:#}", installed.unwrap_err());
        assert!(err.contains("Cannot inspect"), "{err}");
    }

    #[test]
    fn relative_paths_climb_to_the_shared_ancestor() {
        assert_eq!(
            relative(
                Path::new("/h/.claude/skills"),
                Path::new("/h/.agents/skills/user-memory")
            ),
            Path::new("../../.agents/skills/user-memory")
        );
        assert_eq!(
            relative(
                Path::new("/cfg/skills"),
                Path::new("/h/.agents/skills/user-memory")
            ),
            Path::new("../../h/.agents/skills/user-memory")
        );
        assert_ne!(version(FILES), version(NEWER));
        assert_eq!(version(FILES), version(FILES));
    }
}
