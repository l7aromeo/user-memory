//! Stable project identity for a folder, independent of device paths.

use crate::{
    memory::{local_path, project_key, remote_project_key},
    paths::{self, home},
};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
};

mod rekey;

pub use rekey::move_project;

/// Names the project of a folder without git, or of a repository without a
/// remote: one key line. Outside git it covers the folder's subfolders too.
pub const KEY_FILE: &str = ".user-memory-key";

/// Lists, in `@path` lines, further folders whose projects a session in the
/// folder loads.
pub const LIST_FILE: &str = ".user-memory";

/// Resolves the key new memories use for the folder `path`.
///
/// Inside a git repository: the `origin` remote's key, else the git root's
/// `.user-memory-key`, else `git-root:<root commit>`. Outside git: the
/// nearest `.user-memory-key` in `path` or a parent below the home folder.
/// Never falls back to a filesystem path; an error means the folder has no
/// project.
pub fn project_id(path: &Path) -> Result<String> {
    detect(path, home().as_deref())
}

/// A folder's project: the key new memories use, a key file's key that
/// differs from the git remote's and so waits for a move, and what the key
/// file says that counts for nothing.
struct Identity {
    key: String,
    pending: Option<String>,
    notes: Vec<String>,
}

/// [`project_id`] with the home folder given, so tests need not change the
/// process environment.
fn detect(path: &Path, home: Option<&Path>) -> Result<String> {
    resolve(path, home).map(|identity| identity.key)
}

fn resolve(path: &Path, home: Option<&Path>) -> Result<Identity> {
    match git_root(path)? {
        Some(root) => repository_identity(&root),
        None => match nearest(path, home, key_file)? {
            Some(file) => Ok(Identity {
                notes: file.notes(),
                key: file.key,
                pending: None,
            }),
            None => bail!(
                "{} is not in a git repository and no .user-memory-key in it or a parent folder below your home folder names its project",
                path.display()
            ),
        },
    }
}

/// The checkout's `.git` and git's complaint, when `path` is in a checkout
/// git cannot read, such as one whose `.git` file names a repository that
/// is gone. Found by the `.git` entry itself, since git words this
/// differently from version to version. `None` when git is not installed.
pub fn broken_checkout(path: &Path) -> Option<(PathBuf, String)> {
    let dot_git = path
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|entry| fs::symlink_metadata(entry).is_ok())?;
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if out.status.success() {
        return None;
    }
    // The common case, a `.git` file naming a repository that is gone, said
    // plainly; some git versions name no path in their complaint.
    let named = fs::read_to_string(&dot_git).ok().and_then(|text| {
        let target = text
            .lines()
            .next()?
            .strip_prefix("gitdir:")?
            .trim()
            .to_owned();
        let parent = dot_git.parent()?;
        (!parent.join(&target).exists()).then_some(target)
    });
    let problem = match named {
        Some(target) => format!("its .git file names {target}, which does not exist"),
        None => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = stderr.trim();
            stderr.strip_prefix("fatal: ").unwrap_or(stderr).to_owned()
        }
    };
    Some((dot_git, problem))
}

/// The work tree root of the repository holding `path`, or `None` when
/// `path` is outside git or git is not installed.
fn git_root(path: &Path) -> Result<Option<PathBuf>> {
    let out = match Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(out) => out,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("Cannot run git"),
    };
    if out.status.success() {
        // As this system writes it: git on Windows answers with forward
        // slashes (and may use a short 8.3 name), so the root is resolved
        // to the native form, without the \\?\ prefix canonicalize adds.
        let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        return Ok(Some(dunce::canonicalize(&root).unwrap_or(root)));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("not a git repository") {
        return Ok(None);
    }
    bail!(
        "git rev-parse --show-toplevel failed in {}: {}",
        path.display(),
        stderr.trim()
    )
}

/// A line without its trailing ` # comment`, trimmed; `None` for a blank
/// line or a comment.
fn meaningful(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .char_indices()
        .find(|&(i, c)| c == '#' && line[..i].ends_with(char::is_whitespace))
        .map_or(line.len(), |(i, _)| i);
    Some(line[..end].trim_end())
}

/// The lines of the file `name` in `dir` that are not blank or comments, or
/// `None` when there is no such file or it holds none.
fn read_lines(dir: &Path, name: &str) -> Option<Vec<String>> {
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let lines: Vec<String> = text
        .lines()
        .filter_map(meaningful)
        .map(str::to_owned)
        .collect();
    (!lines.is_empty()).then_some(lines)
}

/// What a `.user-memory-key` says.
struct KeyFile {
    /// The file itself.
    file: PathBuf,
    /// Its first line, normalised.
    key: String,
    /// Any further lines, which count for nothing.
    extra: Vec<String>,
}

impl KeyFile {
    fn notes(&self) -> Vec<String> {
        self.extra
            .iter()
            .map(|line| format!("{line}: ignored; {} holds one key", self.file.display()))
            .collect()
    }
}

fn key_file(dir: &Path) -> Option<KeyFile> {
    let mut lines = read_lines(dir, KEY_FILE)?.into_iter();
    let key = project_key(&lines.next()?);
    Some(KeyFile {
        file: dir.join(KEY_FILE),
        key,
        extra: lines.collect(),
    })
}

/// What a `.user-memory` says.
struct ListFile {
    /// The folder holding the file, which its `@path` lines are relative to.
    dir: PathBuf,
    lines: Vec<String>,
}

fn list_file(dir: &Path) -> Option<ListFile> {
    Some(ListFile {
        dir: dir.to_owned(),
        lines: read_lines(dir, LIST_FILE)?,
    })
}

/// Outside git: what `read` finds in `path` or the nearest parent that has
/// it. The search stops before the home folder, so one file there cannot
/// make every folder a single project; the filesystem root never counts
/// either.
fn nearest<T>(path: &Path, home: Option<&Path>, read: fn(&Path) -> Option<T>) -> Result<Option<T>> {
    let path =
        dunce::canonicalize(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let home = home.and_then(|home| dunce::canonicalize(home).ok());
    for dir in path.ancestors() {
        let at_or_above_home = home.as_deref().is_some_and(|home| home.starts_with(dir));
        if at_or_above_home || dir.parent().is_none() {
            break;
        }
        if let Some(found) = read(dir) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The remote's project key for the repository at `root`, if it has one.
fn remote_of(root: &Path) -> Option<String> {
    git(root, &["config", "--get", "remote.origin.url"])
        .ok()
        .and_then(|url| remote_project_key(&url))
}

/// Inside a repository: the remote's key, else the root's key file, else
/// the smallest root commit. A key file that differs from the remote names
/// the project the folder had before it got the remote: it stays pending
/// until its memories move to the remote's key. One that agrees is only
/// redundant.
fn repository_identity(root: &Path) -> Result<Identity> {
    let file = key_file(root);
    let mut notes = file.as_ref().map(KeyFile::notes).unwrap_or_default();
    match (file, remote_of(root)) {
        (Some(file), Some(remote)) if file.key != remote => Ok(Identity {
            key: remote,
            pending: Some(file.key),
            notes,
        }),
        (Some(file), Some(remote)) => {
            notes.push(format!(
                "{} repeats the git remote's key {remote}; delete it",
                file.file.display()
            ));
            Ok(Identity {
                key: remote,
                pending: None,
                notes,
            })
        }
        (None, Some(key)) => Ok(Identity {
            key,
            pending: None,
            notes,
        }),
        (Some(file), None) => Ok(Identity {
            key: file.key,
            pending: None,
            notes,
        }),
        (None, None) => {
            let roots = git(root, &["rev-list", "--max-parents=0", "HEAD"]).context(
                "git repository has no commits, no origin remote and no .user-memory-key",
            )?;
            let first = roots
                .lines()
                .map(str::trim)
                .filter(|sha| !sha.is_empty())
                .min()
                .context("git reported no root commit")?;
            Ok(Identity {
                key: format!("git-root:{first}"),
                pending: None,
                notes,
            })
        }
    }
}

/// Everything that decides which projects a session in a folder loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    /// The key new memories here use, or why the folder has none.
    pub key: Result<String, String>,
    /// A `.user-memory-key` key that differs from the git remote's key: its
    /// memories wait for `user-memory project move <pending> <key>`.
    pub pending: Option<String>,
    /// The `@path` lines of the `.user-memory` that applies, in order.
    pub includes: Vec<Include>,
    /// What the folder's files say that counts for nothing or should go,
    /// worded for the header.
    pub warnings: Vec<String>,
}

/// One `@path` line and the project of the folder it lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    /// The folder as written after `@`: relative to the file's folder,
    /// absolute, or under the home folder with `~/`.
    pub path: PathBuf,
    /// That folder's key for new memories, or why there is none, worded to
    /// follow "@<path>: " in the session header.
    pub key: Result<String, String>,
    /// As [`Folder::pending`], for that folder.
    pub pending: Option<String>,
    /// What that folder's key file says that counts for nothing or should
    /// go, each starting "@<path>: ", worded for the header.
    pub warnings: Vec<String>,
}

/// The projects of the folder `path`: its own key and any pending one, and
/// the folders its `.user-memory` lists, each resolved as [`project_id`]
/// resolves a folder. Never fails; listed folders' own lists are not
/// followed.
pub fn folder(path: &Path) -> Folder {
    folder_with_home(path, home().as_deref())
}

fn folder_with_home(path: &Path, home: Option<&Path>) -> Folder {
    let (key, pending, warnings) = match resolve(path, home) {
        Ok(identity) => (Ok(identity.key), identity.pending, identity.notes),
        Err(err) => (Err(format!("{err:#}")), None, Vec::new()),
    };
    let mut found = Folder {
        key,
        pending,
        includes: Vec::new(),
        warnings,
    };
    let list = match git_root(path) {
        Ok(Some(root)) => list_file(&root),
        Ok(None) => nearest(path, home, list_file).ok().flatten(),
        Err(_) => None,
    };
    let Some(list) = list else {
        return found;
    };
    let mut listed = Vec::new();
    for line in &list.lines {
        match line.strip_prefix('@') {
            Some(folder) => listed.push(folder.trim()),
            None => found
                .warnings
                .push(format!("{line}: a key belongs in .user-memory-key")),
        }
    }
    // Each listed folder costs a few git calls; resolving them side by side
    // keeps a long list within the prompt hook's time limit. Joining in
    // order keeps the file's order.
    found.includes = std::thread::scope(|scope| {
        let running: Vec<_> = listed
            .iter()
            .map(|folder| scope.spawn(|| included(&list.dir, folder, home)))
            .collect();
        running
            .into_iter()
            .zip(&listed)
            .map(|(thread, folder)| {
                thread.join().unwrap_or_else(|_| Include {
                    path: PathBuf::from(folder),
                    key: Err("no project (resolving it failed)".into()),
                    pending: None,
                    warnings: Vec::new(),
                })
            })
            .collect()
    });
    found
}

/// The folder an `@` line names: relative to `dir`, absolute, or under the
/// home folder with `~/` (or `~\` on Windows). Folders outside `dir` are
/// fine: the list file is per machine and never committed, and only the
/// folder's project key is read. `None` for `~` without a home folder.
fn located(dir: &Path, written: &str, home: Option<&Path>) -> Option<PathBuf> {
    if written == "~" {
        return home.map(Path::to_path_buf);
    }
    match written
        .strip_prefix('~')
        .and_then(|rest| rest.strip_prefix(std::path::is_separator))
    {
        Some(rest) => home.map(|home| home.join(rest)),
        // An absolute path replaces `dir` when joined.
        None => Some(dir.join(written)),
    }
}

/// One spelling of the folder `path` names, so `../x`, `x/` and its
/// absolute form compare equal: resolved by the file system where it
/// exists, by removing `.` and `..` by name where it does not.
fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| {
        let mut clean = PathBuf::new();
        for part in path.components() {
            match part {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other),
            }
        }
        clean
    })
}

/// The project of `listed`, a folder as an `@` line in `dir`'s list file
/// writes it, or why there is none.
fn included(dir: &Path, listed: &str, home: Option<&Path>) -> Include {
    let path = PathBuf::from(listed);
    let refused = |why: &str| Include {
        path: path.clone(),
        key: Err(why.into()),
        pending: None,
        warnings: Vec::new(),
    };
    if listed.is_empty() {
        return refused("refused: no folder named");
    }
    let Some(target) = located(dir, listed, home) else {
        return refused("no such folder (no home folder for ~)");
    };
    if !target.is_dir() {
        return refused(if target.exists() {
            "not a folder"
        } else {
            "no such folder"
        });
    }
    match resolve(&target, home) {
        Ok(identity) => Include {
            warnings: identity
                .notes
                .iter()
                .map(|note| format!("@{}: {note}", path.display()))
                .collect(),
            path,
            key: Ok(identity.key),
            pending: identity.pending,
        },
        Err(err) => refused(&match git_root(&target) {
            Ok(Some(_)) => "no project (no commits and no git remote)".into(),
            Ok(None) => "no project (no git remote)".into(),
            Err(_) => format!("no project ({err:#})"),
        }),
    }
}

/// What to do about a key file that waits for a move, for doctor and
/// `project-id`.
pub fn move_advice(pending: &str, key: &str) -> String {
    format!(
        "the .user-memory-key key {pending} differs from the git remote's key {key}: run `user-memory project move {pending} {key}` where the memory store lives, then delete .user-memory-key"
    )
}

/// `raw` as a project key, normalised; refuses an empty key and one that
/// names a place on this machine, which would make the key device-specific.
fn checked_key(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("A project key cannot be empty");
    }
    if raw.starts_with('@') {
        bail!("{raw} lists a folder; a project key cannot start with @");
    }
    if local_path(raw) || raw.starts_with('.') || raw.contains('\\') {
        bail!(
            "{raw} is a local path; name the project with a key such as its planned remote, github.com/<owner>/<name>"
        );
    }
    Ok(project_key(raw))
}

/// Names the project of the folder `path` in its `.user-memory-key`: at the
/// git root inside a repository without a remote, else in `path` itself.
/// Returns the normalised key. Inside a repository with a remote, accepts
/// only the remote's key and writes nothing. Refuses a key that is a local
/// path, a file that already names another project, a symbolic link, and a
/// folder at or above the home folder, where the file would be ignored.
pub fn set(path: &Path, key: &str) -> Result<String> {
    set_with_home(path, key, home().as_deref())
}

fn set_with_home(path: &Path, key: &str, home: Option<&Path>) -> Result<String> {
    let key = checked_key(key)?;
    let root = git_root(path)?;
    if let Some(remote) = root.as_deref().and_then(remote_of) {
        if remote == key {
            // The remote names it already; a key file would add nothing.
            return Ok(key);
        }
        bail!(
            "this repository's git remote already names {remote}; a .user-memory-key naming {key} would only wait for a move to it"
        );
    }
    let dir = match root {
        Some(root) => root,
        None => {
            let dir = dunce::canonicalize(path)
                .with_context(|| format!("Cannot open {}", path.display()))?;
            if let Some(home) = home.and_then(|home| dunce::canonicalize(home).ok())
                && home.starts_with(&dir)
            {
                bail!(
                    "{} is your home folder or above it, where a .user-memory-key is ignored; set the key in the project's own folder",
                    dir.display()
                );
            }
            dir
        }
    };
    let file = dir.join(KEY_FILE);
    if fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!(
            "{} is a symbolic link; user-memory leaves it and its target as they are",
            file.display()
        );
    }
    match key_file(&dir) {
        Some(existing) if existing.key == key => return Ok(key),
        Some(existing) => bail!(
            "{} already names {}; remove it first to change the project",
            file.display(),
            existing.key
        ),
        None => {}
    }
    // Comments already there stay, below the key.
    let rest = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", file.display())),
    };
    fs::write(&file, format!("{key}\n{rest}"))
        .with_context(|| format!("Cannot write {}", file.display()))?;
    Ok(key)
}

/// The folder whose files `project-id` edits for `path`: the git root inside
/// a repository, else `path` itself, refusing the home folder and above,
/// where the files are ignored.
fn own_folder(
    path: &Path,
    root: Option<PathBuf>,
    home: Option<&Path>,
    file: &str,
) -> Result<PathBuf> {
    if let Some(root) = root {
        return Ok(root);
    }
    let dir =
        dunce::canonicalize(path).with_context(|| format!("Cannot open {}", path.display()))?;
    if let Some(home) = home.and_then(|home| dunce::canonicalize(home).ok())
        && home.starts_with(&dir)
    {
        bail!(
            "{} is your home folder or above it, where a {file} is ignored; use the folder you start sessions in",
            dir.display()
        );
    }
    Ok(dir)
}

/// `file` in `dir`, refused when it is a symbolic link.
fn own_file(dir: &Path, file: &str) -> Result<PathBuf> {
    let file = dir.join(file);
    if fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!(
            "{} is a symbolic link; user-memory leaves it and its target as they are",
            file.display()
        );
    }
    Ok(file)
}

/// Whether an `@path` line in `dir`'s list file names the folder `listed`
/// names, however each is written: `app`, `app/`, `../x/app` and the
/// absolute path are one folder.
fn same_folder(dir: &Path, home: Option<&Path>, line: &str, listed: &str) -> bool {
    let folder = |written: &str| located(dir, written, home).map(|path| canonical(&path));
    meaningful(line)
        .and_then(|line| line.strip_prefix('@'))
        .is_some_and(|written| {
            let written = written.trim();
            if written.is_empty() {
                return false;
            }
            let (Some(written), Some(listed)) = (folder(written), folder(listed)) else {
                return false;
            };
            paths::same(&written, &listed)
        })
}

/// What `project-id --include` or `--exclude` did.
pub enum Listed {
    /// The line was added; the folder's project key.
    Added { file: PathBuf, key: String },
    /// The file already listed the folder.
    Already { file: PathBuf },
    /// The line was removed.
    Removed { file: PathBuf },
    /// The file did not list the folder.
    Absent { file: PathBuf },
}

impl Listed {
    pub fn describe(&self, listed: &str) -> String {
        match self {
            Self::Added { file, key } => format!("Listed @{listed} ({key}) in {}", file.display()),
            Self::Already { file } => format!("@{listed} is already listed in {}", file.display()),
            Self::Removed { file } => format!("Removed @{listed} from {}", file.display()),
            Self::Absent { file } => format!("@{listed} is not listed in {}", file.display()),
        }
    }
}

/// Adds an `@listed` line to the `.user-memory` of the folder `path` (the
/// git root inside a repository), unless it is there already in any
/// spelling, keeping every other line as written. Refuses a folder that
/// could not load: a missing folder or one without a project.
pub fn include(path: &Path, listed: &str) -> Result<Listed> {
    include_with_home(path, listed, home().as_deref())
}

fn include_with_home(path: &Path, listed: &str, home: Option<&Path>) -> Result<Listed> {
    let dir = own_folder(path, git_root(path)?, home, LIST_FILE)?;
    let file = own_file(&dir, LIST_FILE)?;
    let target = included(&dir, listed.trim(), home);
    let key = target
        .key
        .map_err(|why| anyhow::anyhow!("@{}: {why}", listed.trim()))?;
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", file.display())),
    };
    if text
        .lines()
        .any(|line| same_folder(&dir, home, line, listed.trim()))
    {
        return Ok(Listed::Already { file });
    }
    let separator = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(&file, format!("{text}{separator}@{}\n", listed.trim()))
        .with_context(|| format!("Cannot write {}", file.display()))?;
    Ok(Listed::Added { file, key })
}

/// Removes every `@listed` line from the `.user-memory` of the folder
/// `path` (the git root inside a repository), keeping every other line as
/// written.
pub fn exclude(path: &Path, listed: &str) -> Result<Listed> {
    exclude_with_home(path, listed, home().as_deref())
}

fn exclude_with_home(path: &Path, listed: &str, home: Option<&Path>) -> Result<Listed> {
    let dir = own_folder(path, git_root(path)?, home, LIST_FILE)?;
    let file = own_file(&dir, LIST_FILE)?;
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Listed::Absent { file }),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", file.display())),
    };
    let kept: Vec<&str> = text
        .split_inclusive('\n')
        .filter(|line| !same_folder(&dir, home, line, listed.trim()))
        .collect();
    if kept.len() == text.split_inclusive('\n').count() {
        return Ok(Listed::Absent { file });
    }
    fs::write(&file, kept.concat()).with_context(|| format!("Cannot write {}", file.display()))?;
    Ok(Listed::Removed { file })
}

/// Runs git with arguments (never through a shell) and returns trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("Cannot run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    /// Git for fixtures, isolated from the user's global and system config so
    /// settings such as commit.gpgsign or core.hooksPath cannot break or hang it.
    /// The global config is an empty file beside `dir`, in the test's own
    /// folder, which works on every platform, where the null device's name
    /// differs.
    fn run(dir: &Path, args: &[&str]) -> String {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let global = dir.with_file_name(format!(".{name}.gitconfig"));
        if !global.exists() {
            fs::write(&global, "").unwrap();
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
    fn repo() -> (TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        (tmp, dir)
    }

    #[test]
    fn only_a_checkout_git_cannot_read_is_broken() {
        let (tmp, repo) = repo();
        assert_eq!(broken_checkout(&repo), None, "a healthy checkout");
        let plain = tmp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        assert_eq!(broken_checkout(&plain), None, "no checkout at all");
        // A `.git` file naming a repository that is gone, found from below.
        let copied = tmp.path().join("copied");
        fs::create_dir_all(copied.join("src")).unwrap();
        fs::write(copied.join(".git"), "gitdir: ../gone/.git\n").unwrap();
        assert_eq!(
            broken_checkout(&copied.join("src")),
            Some((
                copied.join(".git"),
                "its .git file names ../gone/.git, which does not exist".into()
            ))
        );
        // One naming a folder that is there but no repository: git's own
        // words.
        fs::write(copied.join(".git"), "gitdir: ../plain\n").unwrap();
        let (dot_git, problem) = broken_checkout(&copied).unwrap();
        assert_eq!(dot_git, copied.join(".git"));
        assert!(
            !problem.is_empty() && !problem.starts_with("fatal: "),
            "{problem}"
        );
    }

    #[test]
    fn several_root_commits_resolve_to_the_smallest_sha() {
        let (_tmp, dir) = repo();
        run(&dir, &["commit", "-q", "--allow-empty", "-m", "a"]);
        run(&dir, &["checkout", "-q", "--orphan", "other"]);
        run(&dir, &["commit", "-q", "--allow-empty", "-m", "b"]);
        run(
            &dir,
            &[
                "merge",
                "-q",
                "--allow-unrelated-histories",
                "-m",
                "join",
                "main",
            ],
        );
        let roots = run(&dir, &["rev-list", "--max-parents=0", "HEAD"]);
        let mut roots: Vec<_> = roots.lines().collect();
        assert_eq!(roots.len(), 2);
        roots.sort_unstable();
        assert_eq!(project_id(&dir).unwrap(), format!("git-root:{}", roots[0]));
    }

    #[test]
    fn local_path_remotes_fall_back_to_the_root_commit() {
        let (_tmp, dir) = repo();
        run(&dir, &["commit", "-q", "--allow-empty", "-m", "a"]);
        run(&dir, &["remote", "add", "origin", "/srv/git/Repo.git"]);
        let root = run(&dir, &["rev-list", "--max-parents=0", "HEAD"]);
        assert_eq!(project_id(&dir).unwrap(), format!("git-root:{root}"));
    }

    #[test]
    fn blank_override_files_are_ignored_and_values_are_normalized() {
        let (_tmp, dir) = repo();
        run(
            &dir,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Owner/Repo.git",
            ],
        );
        fs::write(dir.join(KEY_FILE), " \n\t\n").unwrap();
        assert_eq!(project_id(&dir).unwrap(), "github.com/owner/repo");
        // Without a remote the key line names the repository.
        run(&dir, &["remote", "remove", "origin"]);
        fs::write(dir.join(KEY_FILE), "\n  Acme   Platform \nnext\n").unwrap();
        assert_eq!(project_id(&dir).unwrap(), "acme platform");
        fs::write(dir.join(KEY_FILE), "git@github.com:Team/Mono.git\n").unwrap();
        assert_eq!(project_id(&dir).unwrap(), "github.com/team/mono");
    }

    #[test]
    fn repositories_without_commits_or_identity_fail_with_a_git_error() {
        let (_tmp, dir) = repo();
        let err = project_id(&dir).unwrap_err().to_string();
        assert!(err.contains("git repository has no commits"), "{err}");
    }

    /// A home folder beside the fixtures, so no fixture lies at or above it.
    fn home(tmp: &TempDir) -> PathBuf {
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn a_key_file_that_differs_from_the_remote_waits_for_a_move() {
        let (tmp, dir) = repo();
        run(
            &dir,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        fs::write(dir.join(KEY_FILE), "local:app\n").unwrap();
        fs::write(dir.join(LIST_FILE), "@vendor\n").unwrap();
        let sub = dir.join("src");
        fs::create_dir_all(&sub).unwrap();
        // New memories use the remote's key; the file's key waits for a move.
        let home = home(&tmp);
        assert_eq!(detect(&sub, Some(&home)).unwrap(), "github.com/o/r");
        let found = folder_with_home(&sub, Some(&home));
        assert_eq!(found.key, Ok("github.com/o/r".into()));
        assert_eq!(found.pending, Some("local:app".into()));
        assert!(found.warnings.is_empty(), "{found:?}");
        assert_eq!(
            keys(&found),
            [("vendor".into(), Err("no such folder".into()))]
        );
        // The same key twice is no move, only a file to delete.
        fs::write(dir.join(KEY_FILE), "https://github.com/O/R\n").unwrap();
        let found = folder_with_home(&sub, Some(&home));
        assert_eq!(found.pending, None);
        assert_eq!(found.warnings.len(), 1, "{found:?}");
        assert!(
            found.warnings[0].ends_with(
                ".user-memory-key repeats the git remote's key github.com/o/r; delete it"
            ),
            "{found:?}"
        );
        // After the move, deleting the file leaves the remote's key alone.
        fs::remove_file(dir.join(KEY_FILE)).unwrap();
        let found = folder_with_home(&sub, Some(&home));
        assert!(found.warnings.is_empty(), "{found:?}");
        assert_eq!(
            (found.key, found.pending),
            (Ok("github.com/o/r".into()), None)
        );
    }

    #[test]
    fn a_listed_folder_whose_key_file_differs_from_its_remote_waits_for_a_move() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/chutao");
        let app = parent.join("app");
        remote_repo(&app, "https://github.com/me/app");
        fs::write(app.join(KEY_FILE), "local:app\n").unwrap();
        fs::write(parent.join(LIST_FILE), "@app\n").unwrap();
        let found = folder_with_home(&parent, Some(&home));
        assert_eq!(
            found.includes,
            [Include {
                path: "app".into(),
                key: Ok("github.com/me/app".into()),
                pending: Some("local:app".into()),
                warnings: Vec::new(),
            }]
        );
        fs::remove_file(app.join(KEY_FILE)).unwrap();
        assert_eq!(
            folder_with_home(&parent, Some(&home)).includes[0].pending,
            None
        );
    }

    #[test]
    fn a_repository_without_a_file_uses_its_remote() {
        let (tmp, dir) = repo();
        run(&dir, &["remote", "add", "origin", "git@github.com:O/R.git"]);
        assert_eq!(detect(&dir, Some(&home(&tmp))).unwrap(), "github.com/o/r");
    }

    #[test]
    fn outside_git_the_nearest_file_in_the_folder_or_a_parent_names_the_project() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let project = home.join("work/new-app");
        let deep = project.join("src/lib");
        fs::create_dir_all(&deep).unwrap();
        fs::write(home.join("work").join(KEY_FILE), "outer\n").unwrap();
        fs::write(project.join(KEY_FILE), "\n  github.com/Me/New-App \n").unwrap();
        assert_eq!(detect(&deep, Some(&home)).unwrap(), "github.com/me/new-app");
        assert_eq!(
            detect(&project, Some(&home)).unwrap(),
            "github.com/me/new-app"
        );
        // A blank file names nothing, so the search goes on upward.
        fs::write(project.join(KEY_FILE), " \n").unwrap();
        assert_eq!(detect(&deep, Some(&home)).unwrap(), "outer");
    }

    #[test]
    fn a_file_in_the_home_folder_or_above_names_no_project() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let folder = home.join("scratch");
        fs::create_dir_all(&folder).unwrap();
        fs::write(home.join(KEY_FILE), "everything\n").unwrap();
        fs::write(tmp.path().join(KEY_FILE), "above home\n").unwrap();
        let err = format!("{:#}", detect(&folder, Some(&home)).unwrap_err());
        assert!(err.contains("no .user-memory"), "{err}");
        let err = format!("{:#}", detect(&home, Some(&home)).unwrap_err());
        assert!(err.contains("no .user-memory"), "{err}");
        // Outside the home folder, a parent that is not above it still counts.
        let elsewhere = tempdir().unwrap();
        let inner = elsewhere.path().join("a/b");
        fs::create_dir_all(&inner).unwrap();
        fs::write(elsewhere.path().join("a").join(KEY_FILE), "srv project\n").unwrap();
        assert_eq!(detect(&inner, Some(&home)).unwrap(), "srv project");
    }

    #[test]
    fn a_parent_folders_file_never_applies_inside_a_repository() {
        let (tmp, dir) = repo();
        fs::write(tmp.path().join(KEY_FILE), "parent key\n").unwrap();
        run(&dir, &["commit", "-q", "--allow-empty", "-m", "a"]);
        let root = run(&dir, &["rev-list", "--max-parents=0", "HEAD"]);
        let home = tempdir().unwrap();
        assert_eq!(
            detect(&dir, Some(home.path())).unwrap(),
            format!("git-root:{root}")
        );
    }

    #[test]
    fn directories_outside_git_fail_without_printing_a_path_key() {
        let tmp = tempdir().unwrap();
        let home = tempdir().unwrap();
        let err = format!("{:#}", detect(tmp.path(), Some(home.path())).unwrap_err());
        assert!(
            err.contains("is not in a git repository and no .user-memory"),
            "{err}"
        );
        assert!(!err.contains("git rev-parse"), "{err}");
    }

    // ─── .user-memory lines and includes ────────────────────────────────

    /// A git repository at `dir` with an origin remote and one commit.
    fn remote_repo(dir: &Path, remote: &str) {
        fs::create_dir_all(dir).unwrap();
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["remote", "add", "origin", remote]);
    }

    fn keys(found: &Folder) -> Vec<(String, Result<String, String>)> {
        found
            .includes
            .iter()
            .map(|include| (include.path.display().to_string(), include.key.clone()))
            .collect()
    }

    #[test]
    fn each_file_holds_one_kind_of_line_and_says_what_else_counts_for_nothing() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let dir = home.join("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(KEY_FILE),
            "# the work folder\n\n  github.com/Me/Work   # its own key\nsecond key\n",
        )
        .unwrap();
        fs::write(dir.join(LIST_FILE), "# folders\n@api\nplain line\n").unwrap();
        assert_eq!(detect(&dir, Some(&home)).unwrap(), "github.com/me/work");
        let found = folder_with_home(&dir, Some(&home));
        assert_eq!(keys(&found), [("api".into(), Err("no such folder".into()))]);
        assert_eq!(found.warnings.len(), 2, "{found:?}");
        assert!(
            found.warnings[0].starts_with("second key: ignored; ")
                && found.warnings[0].ends_with(".user-memory-key holds one key"),
            "{found:?}"
        );
        assert_eq!(
            found.warnings[1],
            "plain line: a key belongs in .user-memory-key"
        );
    }

    #[test]
    fn a_list_file_neither_names_nor_hides_a_key() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let dir = home.join("work/chutao");
        fs::create_dir_all(&dir).unwrap();
        fs::write(home.join("work").join(KEY_FILE), "outer\n").unwrap();
        fs::write(dir.join(LIST_FILE), "# repositories\n@djs\n").unwrap();
        assert_eq!(detect(&dir, Some(&home)).unwrap(), "outer");
        // A key file of comments alone names nothing: the search goes on upward.
        fs::write(dir.join(KEY_FILE), "# nothing yet\n\n").unwrap();
        assert_eq!(detect(&dir, Some(&home)).unwrap(), "outer");
    }

    #[test]
    fn includes_resolve_each_listed_folder_through_its_own_remote() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/chutao");
        remote_repo(
            &parent.join("chutao-djs"),
            "git@github.com:Me/ChuTao-DJS.git",
        );
        remote_repo(
            &parent.join("chutao-api"),
            "https://github.com/me/chutao-api",
        );
        let plain = parent.join("notes");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join(KEY_FILE), "Team Notes\n").unwrap();
        fs::write(
            parent.join(LIST_FILE),
            "@chutao-djs   # the server\n@chutao-api/\n@notes\n",
        )
        .unwrap();
        let found = folder_with_home(&parent, Some(&home));
        assert_eq!(
            keys(&found),
            [
                ("chutao-djs".into(), Ok("github.com/me/chutao-djs".into())),
                ("chutao-api/".into(), Ok("github.com/me/chutao-api".into())),
                ("notes".into(), Ok("team notes".into())),
            ]
        );
        // A subfolder outside git shares its parent's file.
        let sub = parent.join("docs");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(keys(&folder_with_home(&sub, Some(&home))), keys(&found));
    }

    #[test]
    fn includes_that_cannot_be_resolved_are_warnings_in_their_place() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/mix");
        let bare = parent.join("no-remote");
        fs::create_dir_all(&bare).unwrap();
        run(&bare, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(parent.join("plain")).unwrap();
        remote_repo(&parent.join("good"), "https://github.com/me/good");
        fs::write(
            parent.join(LIST_FILE),
            "@missing\n@no-remote\n@plain\n@../gone\n@~/gone\n@\n@good\n",
        )
        .unwrap();
        let no_project = |why: &str| Err(format!("no project ({why})"));
        assert_eq!(
            keys(&folder_with_home(&parent, Some(&home))),
            [
                ("missing".into(), Err("no such folder".into())),
                (
                    "no-remote".into(),
                    no_project("no commits and no git remote")
                ),
                ("plain".into(), no_project("no git remote")),
                ("../gone".into(), Err("no such folder".into())),
                ("~/gone".into(), Err("no such folder".into())),
                ("".into(), Err("refused: no folder named".into())),
                ("good".into(), Ok("github.com/me/good".into())),
            ]
        );
    }

    #[test]
    fn inside_a_repository_only_its_own_root_file_lists_includes() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/chutao");
        let repo = parent.join("chutao-djs");
        remote_repo(&repo, "https://github.com/me/chutao-djs");
        remote_repo(
            &parent.join("chutao-api"),
            "https://github.com/me/chutao-api",
        );
        fs::write(parent.join(LIST_FILE), "@chutao-api\n").unwrap();
        let sub = repo.join("src");
        fs::create_dir_all(&sub).unwrap();
        assert!(folder_with_home(&sub, Some(&home)).includes.is_empty());
        fs::write(repo.join(LIST_FILE), "@../chutao-api\n@vendor\n").unwrap();
        remote_repo(&repo.join("vendor"), "https://github.com/other/vendor");
        assert_eq!(
            keys(&folder_with_home(&sub, Some(&home))),
            [
                (
                    "../chutao-api".into(),
                    Ok("github.com/me/chutao-api".into())
                ),
                ("vendor".into(), Ok("github.com/other/vendor".into())),
            ]
        );
    }

    // A folder outside the file's folder loads too, written relative to the
    // file's folder, absolute, or from the home folder: the file is per
    // machine, and only the folder's project key is read.
    #[test]
    fn includes_may_name_folders_outside_the_file_s_folder() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/chutao");
        fs::create_dir_all(&parent).unwrap();
        let infra = home.join("Projects/meoserver-infra");
        remote_repo(&infra, "https://github.com/me/meoserver-infra");
        let notes = home.join("notes");
        remote_repo(&notes, "https://github.com/me/notes");
        let tools = tmp.path().join("elsewhere/tools");
        remote_repo(&tools, "https://github.com/me/tools");
        let tools_line = format!("@{}", tools.display());
        // A symbolic link to a folder counts as that folder. Windows makes
        // one only with Developer Mode or as an administrator, so that line
        // is checked on macOS and Linux.
        let link_line = if cfg!(unix) { "@infra-link\n" } else { "" };
        #[cfg(unix)]
        std::os::unix::fs::symlink(&infra, parent.join("infra-link")).unwrap();
        fs::write(parent.join("readme.txt"), "not a folder\n").unwrap();
        fs::write(
            parent.join(LIST_FILE),
            format!("@../meoserver-infra\n@~/notes\n{tools_line}\n{link_line}@readme.txt\n"),
        )
        .unwrap();
        let mut expected = vec![
            (
                "../meoserver-infra".into(),
                Ok("github.com/me/meoserver-infra".into()),
            ),
            ("~/notes".into(), Ok("github.com/me/notes".into())),
            (
                tools.display().to_string(),
                Ok("github.com/me/tools".into()),
            ),
        ];
        if cfg!(unix) {
            expected.push((
                "infra-link".into(),
                Ok("github.com/me/meoserver-infra".into()),
            ));
        }
        expected.push(("readme.txt".into(), Err("not a folder".into())));
        assert_eq!(keys(&folder_with_home(&parent, Some(&home))), expected);
    }

    // `--include` takes an outside folder in any of its forms, and one
    // folder written two ways is listed once; `--exclude` finds it by
    // either form.
    #[test]
    fn including_an_outside_folder_lists_it_once_in_any_form() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/chutao");
        fs::create_dir_all(&parent).unwrap();
        let infra = home.join("Projects/meoserver-infra");
        remote_repo(&infra, "https://github.com/me/meoserver-infra");
        let file = parent.join(LIST_FILE);
        let added = include_with_home(&parent, "../meoserver-infra", Some(&home)).unwrap();
        assert!(
            matches!(added, Listed::Added { ref key, .. } if key == "github.com/me/meoserver-infra")
        );
        for same in [
            infra.display().to_string(),
            "~/Projects/meoserver-infra".to_owned(),
            "../meoserver-infra/".to_owned(),
        ] {
            let again = include_with_home(&parent, &same, Some(&home)).unwrap();
            assert!(matches!(again, Listed::Already { .. }), "{same}");
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), "@../meoserver-infra\n");
        let removed =
            exclude_with_home(&parent, &infra.display().to_string(), Some(&home)).unwrap();
        assert!(matches!(removed, Listed::Removed { .. }));
        assert_eq!(fs::read_to_string(&file).unwrap(), "");
    }

    #[test]
    fn a_file_in_the_home_folder_lists_no_includes() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        remote_repo(&home.join("app"), "https://github.com/me/app");
        fs::write(home.join(LIST_FILE), "@app\n").unwrap();
        let work = home.join("work");
        fs::create_dir_all(&work).unwrap();
        assert!(folder_with_home(&work, Some(&home)).includes.is_empty());
        assert!(folder_with_home(&home, Some(&home)).includes.is_empty());
    }

    #[test]
    fn a_listed_folders_key_file_warnings_name_that_folder() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/workspace");
        let app = parent.join("app");
        remote_repo(&app, "https://github.com/me/app");
        fs::write(app.join(KEY_FILE), "github.com/me/app\n").unwrap();
        let notes = parent.join("notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join(KEY_FILE), "team notes\nsecond\n").unwrap();
        fs::write(parent.join(LIST_FILE), "@app\n@notes\n").unwrap();
        let found = folder_with_home(&parent, Some(&home));
        let app_file = dunce::canonicalize(&app).unwrap().join(KEY_FILE);
        assert_eq!(
            found.includes[0].warnings,
            [format!(
                "@app: {} repeats the git remote's key github.com/me/app; delete it",
                app_file.display()
            )]
        );
        assert_eq!(found.includes[1].warnings.len(), 1, "{found:?}");
        assert!(
            found.includes[1].warnings[0].starts_with("@notes: second: ignored; "),
            "{found:?}"
        );
        // The folder's own warnings stay its own.
        assert!(found.warnings.is_empty(), "{found:?}");
    }

    #[test]
    fn many_listed_folders_come_back_in_the_files_order() {
        let tmp = tempdir().unwrap();
        let home = home(&tmp);
        let parent = home.join("Projects/many");
        let mut list = String::new();
        for i in 0..20 {
            remote_repo(
                &parent.join(format!("r{i}")),
                &format!("https://github.com/me/r{i}"),
            );
            list.push_str(&format!("@r{i}\n"));
        }
        fs::write(parent.join(LIST_FILE), list).unwrap();
        let found = folder_with_home(&parent, Some(&home));
        let want: Vec<_> = (0..20)
            .map(|i| (format!("r{i}"), Ok(format!("github.com/me/r{i}"))))
            .collect();
        assert_eq!(keys(&found), want);
    }

    #[test]
    fn a_tilde_line_is_under_the_home_folder() {
        let (dir, home) = (Path::new("/work"), Path::new("/home/u"));
        assert_eq!(located(dir, "~", Some(home)), Some(home.to_path_buf()));
        assert_eq!(
            located(dir, "~/notes", Some(home)),
            Some(home.join("notes"))
        );
        assert_eq!(located(dir, "~notes", Some(home)), Some(dir.join("~notes")));
        assert_eq!(located(dir, "~/notes", None), None);
        // Windows also writes a backslash, which elsewhere is part of a name.
        let backslash = located(dir, "~\\notes", Some(home));
        if cfg!(windows) {
            assert_eq!(backslash, Some(home.join("notes")));
        } else {
            assert_eq!(backslash, Some(dir.join("~\\notes")));
        }
    }
}
