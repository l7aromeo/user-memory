//! Release tags in a checkout, for a condition that ends when its project
//! has a tag (#121). Read with git on this machine only, never the
//! network.
//!
//! The obvious commands are wrong on a case-insensitive file system: with
//! loose refs, `rev-parse`, `show-ref` and `cat-file` report `V1.2.3`
//! present when only `v1.2.3` exists, and the answer flips once `git gc`
//! packs the refs. So a lookup takes two steps: one batched
//! `cat-file --batch-check` for every name, whose "missing" answers are
//! exact, then, for the names it reports found, `for-each-ref` with each
//! full ref name compared byte for byte.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

/// Tags the record-time echo shows as the latest in a checkout, newest
/// first: enough to see the project's naming, few enough for one line.
pub const LATEST_TAGS: usize = 3;

/// A tag a checkout has, and the commit it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub tag: String,
    pub commit: String,
}

/// The git folder every worktree of `folder`'s repository shares, where
/// its refs live; none outside git or without git.
pub fn common_dir(folder: &Path) -> Option<PathBuf> {
    let out = git(folder)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(dunce::canonicalize(&dir).unwrap_or(dir))
}

/// The modification times, as text, of the files a new tag changes in the
/// common git folder `common`: `packed-refs`, `refs/tags` and each
/// subfolder of it a name in `names` lives in, and reftable's
/// `tables.list`. A missing file is `-`. A stamp is a trigger to look,
/// never the answer: a change means one lookup.
pub fn stamps(common: &Path, names: &[&str]) -> Vec<(String, String)> {
    let mut paths: Vec<String> = vec![
        "packed-refs".to_owned(),
        "refs/tags".to_owned(),
        "reftable/tables.list".to_owned(),
    ];
    for name in names {
        let mut folder = String::from("refs/tags");
        let parts: Vec<&str> = name.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            folder.push('/');
            folder.push_str(part);
            paths.push(folder.clone());
        }
    }
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            let stamp = fs::metadata(common.join(&path))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or_else(|| "-".to_owned(), |t| t.as_nanos().to_string());
            (path, stamp)
        })
        .collect()
}

/// Which of `names` the checkout at `folder` has as tags, exactly as
/// written, each with the commit it points at (an annotated tag peeled).
/// None when git can't be run there.
pub fn found(folder: &Path, names: &[&str]) -> Option<Vec<Found>> {
    if names.is_empty() {
        return Some(Vec::new());
    }
    let mut child = git(folder)
        .args(["cat-file", "--batch-check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut input = String::new();
    for name in names {
        input.push_str(&format!("refs/tags/{name}^{{commit}}\n"));
    }
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let maybe: Vec<(&str, &str)> = names
        .iter()
        .zip(text.lines())
        .filter_map(|(name, line)| {
            let mut words = line.split(' ');
            let commit = words.next()?;
            (words.next() == Some("commit")).then_some((*name, commit))
        })
        .collect();
    if maybe.is_empty() {
        return Some(Vec::new());
    }
    let out = git(folder)
        .arg("for-each-ref")
        .arg("--format=%(refname)")
        .args(maybe.iter().map(|(name, _)| format!("refs/tags/{name}")))
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let refs = String::from_utf8_lossy(&out.stdout);
    let exact: Vec<&str> = refs.lines().collect();
    Some(
        maybe
            .into_iter()
            .filter(|(name, _)| exact.contains(&format!("refs/tags/{name}").as_str()))
            .map(|(name, commit)| Found {
                tag: name.to_owned(),
                commit: commit.to_owned(),
            })
            .collect(),
    )
}

/// The checkout's [`LATEST_TAGS`] newest tags by creation date; empty
/// when it has none or git can't be run there.
pub fn latest(folder: &Path) -> Vec<String> {
    let Ok(out) = git(folder)
        .args([
            "for-each-ref",
            "--sort=-creatordate",
            &format!("--count={LATEST_TAGS}"),
            "--format=%(refname:strip=2)",
            "refs/tags",
        ])
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Git in `folder`, never through a shell.
fn git(folder: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(folder);
    command
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests fail loudly on a broken setup")]
    use super::*;
    use tempfile::tempdir;

    /// Git for fixtures, isolated from the user's own configuration.
    fn run(dir: &Path, args: &[&str]) {
        let global = dir.with_file_name(".tags-test.gitconfig");
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
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("repo");
        fs::create_dir(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["commit", "-q", "--allow-empty", "-m", "start"]);
        (tmp, dir)
    }

    #[test]
    fn a_tag_is_found_exactly_as_written_loose_or_packed() {
        let (_tmp, dir) = repo();
        run(&dir, &["tag", "v1.2.3"]);
        run(&dir, &["tag", "-a", "-m", "release", "v2.0.0"]);
        run(&dir, &["tag", "rel/v3"]);
        for packed in [false, true] {
            if packed {
                run(&dir, &["pack-refs", "--all"]);
            }
            let found = found(&dir, &["v1.2.3", "V1.2.3", "v2.0.0", "v9", "rel/v3"]).unwrap();
            let tags: Vec<&str> = found.iter().map(|f| f.tag.as_str()).collect();
            assert_eq!(tags, ["v1.2.3", "v2.0.0", "rel/v3"], "packed {packed}");
            // An annotated tag counts at the commit it points at.
            assert_eq!(found[0].commit, found[1].commit);
            assert_eq!(found[0].commit.len(), 40);
        }
    }

    #[test]
    fn a_new_tag_changes_a_stamp() {
        let (_tmp, dir) = repo();
        let common = common_dir(&dir).unwrap();
        let before = stamps(&common, &["rel/v3"]);
        assert!(before.iter().any(|(path, _)| path == "refs/tags/rel"));
        // Whole-second file systems need the tag a moment later.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        run(&dir, &["tag", "rel/v3"]);
        assert_ne!(stamps(&common, &["rel/v3"]), before);
    }

    #[test]
    fn the_latest_tags_come_newest_first() {
        let (_tmp, dir) = repo();
        assert!(latest(&dir).is_empty());
        for (n, tag) in ["v0.1.0", "v0.2.0", "v0.3.0", "v0.4.0"].iter().enumerate() {
            let date = format!("2026-01-0{} 00:00:00 +0000", n + 1);
            let global = dir.with_file_name(".tags-test.gitconfig");
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=Test", "-c", "user.email=t@example.invalid"])
                .args(["tag", "-a", "-m", tag, tag])
                .env("GIT_COMMITTER_DATE", &date)
                .env("GIT_CONFIG_GLOBAL", &global)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        assert_eq!(latest(&dir), ["v0.4.0", "v0.3.0", "v0.2.0"]);
    }
}
