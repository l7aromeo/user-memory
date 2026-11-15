//! What a running session last saw of memory, and the notice that tells it
//! what changed since. Each session's snapshot is a file under the user's
//! state directory, written at session start and after each prompt.

use crate::{
    memory::{Position, Stamp, Stamps},
    paths, private,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Changes listed in one notice; the rest are counted, as the index lists them.
const NOTICE_LINES: usize = 10;
/// The most projects a notice names, the rest counted, as session start names
/// at most 20 repositories: a conversation that links many at once still
/// gets a notice well within its client's limit.
const NOTICE_PROJECTS: usize = 20;
/// Snapshots untouched this long belong to sessions long over; writing any
/// snapshot removes them.
const SNAPSHOT_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Session ids are short; a longer one is not a client's and is not used in
/// a file name.
const SESSION_ID_CHARS: usize = 128;
/// Says why a memory that is still active left the notice's session: its
/// project no longer loads there.
const LEFT_WITH_PROJECT: &str = "project no longer loaded";
/// Opens the notice; the agent reads a changed memory before applying it.
const HEADER: &str = "Memory changed since this session last looked (+ new, ~ changed, - forgotten or replaced). Read + and ~ before applying them; stop following - ones, even where your instruction files, loaded at session start, still list them.";

/// Where a project the session sees comes from, as the header labels it:
/// the folder's own key, the key of its `.user-memory-key` that waits for a
/// move, a folder its `.user-memory` lists, or a conversation link.
pub(super) const THIS_FOLDER: &str = "this folder";
pub(super) const PENDING: &str = "from .user-memory-key, move pending";
pub(super) const LISTED: &str = "listed in .user-memory";
pub(super) const LINKED: &str = "linked";

/// One project a session sees, and where it comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Project {
    pub key: String,
    pub source: String,
    /// The working folders the session saw it from (none for a link, or in
    /// a snapshot saved before folders were kept).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
}

/// The projects a session sees after a prompt in `folder`: those it sees
/// there now, each remembering the folders it was seen from, then those it
/// saw before from another folder, which a change of folder never drops. A
/// project leaves when no folder that gave it gives it any more, or, for a
/// link, when the link is gone.
pub(super) fn kept(fresh: Vec<Project>, before: Option<&[Project]>, folder: &str) -> Vec<Project> {
    let before = before.unwrap_or_default();
    let mut projects: Vec<Project> = fresh
        .into_iter()
        .map(|mut project| {
            if project.source != LINKED {
                project.folders.push(folder.to_owned());
            }
            if let Some(old) = before.iter().find(|old| old.key == project.key) {
                for seen in &old.folders {
                    if !project.folders.contains(seen) {
                        project.folders.push(seen.clone());
                    }
                }
            }
            project
        })
        .collect();
    for old in before {
        if projects.iter().any(|p| p.key == old.key) {
            continue;
        }
        let folders: Vec<String> = old
            .folders
            .iter()
            .filter(|f| *f != folder)
            .cloned()
            .collect();
        if !folders.is_empty() {
            projects.push(Project {
                folders,
                ..old.clone()
            });
        }
    }
    projects
}

/// The keys a session saw before and no longer does.
pub(super) fn gone(old: &Snapshot, new: &Snapshot) -> Vec<String> {
    let (Some(old), Some(new)) = (&old.projects, &new.projects) else {
        return Vec::new();
    };
    old.iter()
        .filter(|o| new.iter().all(|n| n.key != o.key))
        .map(|o| o.key.clone())
        .collect()
}

/// What a session saw: its projects, unknown in a snapshot saved before
/// they were, and its memories.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub projects: Option<Vec<Project>>,
    pub stamps: Stamps,
    /// The store's position for these stamps, kept as the store wrote it;
    /// none from a server that knows no positions, or in a snapshot saved
    /// before them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Position>,
}

/// Hex characters of the store key in a snapshot's file name: 64 bits of
/// the SHA-256 of the store's identity, so two stores on one machine never
/// share a key, without a path or address in the name.
const STORE_KEY_CHARS: usize = 16;

/// The snapshot's file name for `client`'s session `session_id` of the
/// store named `store`, or `None` when the id could not safely name a file.
fn file_name(client: &str, session_id: &str, store: &str) -> Option<String> {
    let safe = !session_id.is_empty()
        && session_id.len() <= SESSION_ID_CHARS
        && !session_id.starts_with('.')
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let key = store_key(store);
    safe.then(|| format!("{client}-{session_id}-{key}.json"))
}

/// The key that names the files of the store named `store` in the state
/// folder: its session snapshots and the rules it last showed.
pub(super) fn store_key(store: &str) -> String {
    crate::setup::sha256(store.as_bytes())[..STORE_KEY_CHARS].to_owned()
}

/// The folder of the session snapshots and the rules each store last
/// showed.
pub(super) fn sessions_dir() -> Option<PathBuf> {
    Some(paths::state_dir()?.join("sessions"))
}

/// The snapshot file of `client`'s session `session_id` of the store named
/// `store`. A session's snapshot of another store, such as a build run
/// against a store of its own, is another file, never compared with it.
pub(super) fn snapshot_path(client: &str, session_id: &str, store: &str) -> Option<PathBuf> {
    Some(sessions_dir()?.join(file_name(client, session_id, store)?))
}

/// The snapshot saved at `path`, if one is there and readable. Snapshots
/// from before their file name held the store's key are never read: which
/// store they saw is unknown.
pub(super) fn load(path: &Path) -> Option<Snapshot> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Age past which a temporary snapshot file is left over from a save its
/// hook's time limit cut off: a save takes milliseconds.
const LEFTOVER_AGE: Duration = Duration::from_secs(60);
/// The end of a temporary snapshot file's name.
const TEMPORARY: &str = ".partial";

/// Whether a save may still replace the session's snapshot. A hook past its
/// time limit abandons the save, and the snapshot stays what the session was
/// last told about; a save that has replaced it can no longer be abandoned,
/// so the hook then tells what it saved. Without this, a save finishing as
/// the limit passes would record changes the session never heard of.
#[derive(Default)]
pub(super) struct Landing(std::sync::Mutex<Option<bool>>);

impl Landing {
    /// Runs `land` unless the save was abandoned, and records that it landed.
    fn land(&self, land: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        let mut landed = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *landed == Some(false) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        land()?;
        *landed = Some(true);
        Ok(())
    }

    /// Abandons the save unless it already landed, and says whether it did.
    pub(super) fn abandon(&self) -> bool {
        let mut landed = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *landed != Some(true) {
            *landed = Some(false);
        }
        *landed == Some(true)
    }
}

/// Writes `snapshot` to `out` in one write: encoded whole first, since a
/// snapshot holds a stamp for every memory the session sees (822 KB at
/// 10,000 memories), and encoding straight into a file made a system call of
/// each small piece.
fn write_whole(mut out: impl io::Write, snapshot: &Snapshot) -> io::Result<()> {
    out.write_all(&serde_json::to_vec(snapshot)?)
}

/// Saves `snapshot` at `path` (folders and file private, replaced whole)
/// unless `landing` is abandoned first, then removes the snapshots of
/// sessions over [`SNAPSHOT_AGE`] ago and leftover temporary files.
pub(super) fn save(path: &Path, snapshot: &Snapshot, landing: &Landing) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    private::create_dir(dir)?;
    let name = path
        .file_name()
        .ok_or(io::ErrorKind::InvalidInput)?
        .to_string_lossy();
    let temporary = dir.join(format!(".{name}.{}{TEMPORARY}", std::process::id()));
    let written = (|| {
        let file = private::create_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            &temporary,
        )?;
        write_whole(file, snapshot)?;
        #[cfg(debug_assertions)]
        hold_for_tests(HOLD_SNAPSHOT);
        landing.land(|| fs::rename(&temporary, path))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written?;
    remove_old(dir, SystemTime::now());
    Ok(())
}

/// The extension of the file beside a session's snapshot that says its
/// start could not read memory, so it shares the snapshot's name and age.
const MISSED: &str = "missed";

/// The file that says the session whose snapshot is at `snapshot` started
/// without memory and has not had the index since.
pub(super) fn missed_path(snapshot: &Path) -> PathBuf {
    snapshot.with_extension(MISSED)
}

/// Marks at `path` that a session's start has not read memory (an empty
/// private file).
pub(super) fn mark_missed(path: &Path) -> io::Result<()> {
    private::create_dir(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    private::create_file(fs::OpenOptions::new().write(true).create(true), path).map(drop)
}

/// Names, in debug builds only, the file that holds the index a prompt asks
/// for a session that started without memory ([`hold_for_tests`]).
#[cfg(debug_assertions)]
pub(crate) const HOLD_LATE_INDEX: &str = "USER_MEMORY_HOLD_LATE_INDEX";

/// Names, in debug builds only, the file that holds a save before it
/// replaces the snapshot ([`hold_for_tests`]).
#[cfg(debug_assertions)]
const HOLD_SNAPSHOT: &str = "USER_MEMORY_HOLD_SNAPSHOT";
/// Names, in debug builds only, the file that holds the prompt hook past its
/// limit before it abandons the save ([`hold_for_tests`]).
#[cfg(debug_assertions)]
pub(crate) const HOLD_ABANDON: &str = "USER_MEMORY_HOLD_ABANDON";

/// In debug builds, waits here while the file the environment variable `var`
/// names exists, after creating that name with `.reached` added, so a test
/// can order a save against its hook.
#[cfg(debug_assertions)]
pub(super) fn hold_for_tests(var: &str) {
    let Some(hold) = std::env::var_os(var) else {
        return;
    };
    let mut reached = hold.clone();
    reached.push(".reached");
    let _ = fs::write(&reached, "");
    while Path::new(&hold).exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Removes the snapshots and [`MISSED`] marks in `dir` last written over
/// [`SNAPSHOT_AGE`] before `now`, and the temporary files of saves cut off
/// over [`LEFTOVER_AGE`] before it. The rules each store last showed are
/// left to session start's own clean-up ([`super::seen::update`]), which
/// never removes the file of the store it is starting.
fn remove_old(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let old = if name.starts_with('.') && name.ends_with(TEMPORARY) {
            age > LEFTOVER_AGE
        } else if super::seen::is_seen_file(&name) {
            false
        } else {
            age > SNAPSHOT_AGE && path.extension().is_some_and(|e| e == "json" || e == MISSED)
        };
        if old {
            let _ = fs::remove_file(path);
        }
    }
}

/// Why a project the session saw no longer loads, by where it came from.
fn dropped(source: &str) -> &'static str {
    match source {
        LINKED => "unlinked",
        LISTED => "no longer listed",
        PENDING => "moved",
        _ => "no longer this folder's",
    }
}

/// The line naming the projects a session gained and lost, or `None` when
/// they are the same or what it saw before is unknown.
fn projects_changed(old: Option<&[Project]>, new: Option<&[Project]>) -> Option<String> {
    let (old, new) = (old?, new?);
    let missing = |from: &[Project], other: &[Project]| -> Vec<Project> {
        from.iter()
            .filter(|p| other.iter().all(|o| o.key != p.key))
            .cloned()
            .collect()
    };
    let parts: Vec<String> = missing(new, old)
        .iter()
        .map(|p| format!("+ {} ({})", p.key, p.source))
        .chain(
            missing(old, new)
                .iter()
                .map(|p| format!("- {} ({})", p.key, dropped(&p.source))),
        )
        .collect();
    if parts.is_empty() {
        return None;
    }
    let listed = parts[..parts.len().min(NOTICE_PROJECTS)].join("; ");
    Some(match parts.len().checked_sub(NOTICE_PROJECTS) {
        Some(rest @ 1..) => format!("Projects changed: {listed}; and {rest} more"),
        _ => format!("Projects changed: {listed}"),
    })
}

/// The notice for a session that last saw `old` and now sees `new`, or
/// `None` when nothing changed: the projects it gained and lost, then
/// memories added, changed and no longer active, the first
/// [`NOTICE_LINES`] listed and the rest counted.
///
/// A memory in `left` is still active but left with its project, so it is
/// marked `<` with its title rather than `-`.
///
/// A memory in `own` was changed last by this conversation, which knows it
/// already, and is left out.
pub(super) fn notice(
    old: &Snapshot,
    new: &Snapshot,
    left: &BTreeSet<String>,
    own: &BTreeSet<String>,
) -> Option<String> {
    let projects = projects_changed(old.projects.as_deref(), new.projects.as_deref());
    // A snapshot saved before digests holds each memory's last update in
    // their place: compared with a digest, every memory would read as
    // changed, so the first notice after an upgrade compares status and
    // title alone.
    let digests = old.changes.is_some() == new.changes.is_some();
    let memories = memory_notice(&old.stamps, &new.stamps, left, own, digests);
    match (projects, memories) {
        (None, None) => None,
        (projects, memories) => Some(
            projects
                .into_iter()
                .chain(memories)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

/// The memory lines of a notice, or `None` when no memory changed.
fn memory_notice(
    old: &Stamps,
    new: &Stamps,
    left: &BTreeSet<String>,
    own: &BTreeSet<String>,
    digests: bool,
) -> Option<String> {
    let short = |id: &str| id.chars().take(8).collect::<String>();
    let line =
        |sign: char, id: &str, stamp: &Stamp| format!("{sign} {} {}", short(id), stamp.title);
    let theirs = |id: &&String| !own.contains(*id);
    let added = new
        .iter()
        .filter(|(id, _)| theirs(id) && !old.contains_key(*id))
        .map(|(id, stamp)| line('+', id, stamp));
    let changed = new
        .iter()
        .filter(|(id, stamp)| {
            theirs(id)
                && old.get(*id).is_some_and(|was| {
                    (digests && was.digest != stamp.digest)
                        || was.status != stamp.status
                        || was.title != stamp.title
                })
        })
        .map(|(id, stamp)| line('~', id, stamp));
    let removed = old
        .iter()
        .filter(|(id, _)| theirs(id) && !new.contains_key(*id))
        .map(|(id, stamp)| {
            if left.contains(id) {
                format!("< {} {} ({LEFT_WITH_PROJECT})", short(id), stamp.title)
            } else {
                format!("- {} {}", short(id), stamp.title)
            }
        });
    let changes: Vec<String> = added.chain(changed).chain(removed).collect();
    if changes.is_empty() {
        return None;
    }
    let mut lines = vec![HEADER.to_owned()];
    lines.extend(changes.iter().take(NOTICE_LINES).cloned());
    if changes.len() > NOTICE_LINES {
        lines.push(format!(
            "and {} more: call index",
            changes.len() - NOTICE_LINES
        ));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(updated_at: &str, status: &str, title: &str) -> Stamp {
        Stamp {
            digest: updated_at.into(),
            status: status.into(),
            title: title.into(),
        }
    }

    fn stamps(entries: &[(&str, Stamp)]) -> Stamps {
        entries
            .iter()
            .map(|(id, s)| ((*id).to_owned(), s.clone()))
            .collect()
    }

    #[test]
    fn a_notice_lists_what_was_added_changed_and_removed() {
        let kept = stamp("2026-10-01 10:00:00", "confirmed", "Prefer Rust");
        let old = stamps(&[
            ("11111111-aaaa", kept.clone()),
            (
                "22222222-bbbb",
                stamp("2026-10-01 10:00:00", "tentative", "Likes tea"),
            ),
            (
                "33333333-cccc",
                stamp("2026-10-01 10:00:00", "confirmed", "Answer briefly"),
            ),
            (
                "44444444-dddd",
                stamp("2026-10-01 10:00:00", "confirmed", "Old title"),
            ),
        ]);
        assert_eq!(
            memory_notice(&old, &old, &BTreeSet::new(), &BTreeSet::new(), true),
            None
        );
        let new = stamps(&[
            ("11111111-aaaa", kept),
            (
                "22222222-bbbb",
                stamp("2026-10-01 10:00:00", "confirmed", "Likes tea"),
            ),
            (
                "44444444-dddd",
                stamp("2026-10-02 09:00:00", "confirmed", "New title"),
            ),
            (
                "55555555-eeee",
                stamp("2026-10-02 09:00:00", "confirmed", "Ask before pushing"),
            ),
        ]);
        assert_eq!(
            memory_notice(&old, &new, &BTreeSet::new(), &BTreeSet::new(), true).unwrap(),
            [
                HEADER,
                "+ 55555555 Ask before pushing",
                "~ 22222222 Likes tea",
                "~ 44444444 New title",
                "- 33333333 Answer briefly",
            ]
            .join("\n")
        );
    }

    #[test]
    fn a_long_notice_counts_what_it_does_not_list() {
        let new: Stamps = (0..NOTICE_LINES + 3)
            .map(|i| {
                (
                    format!("{i:08}-id"),
                    stamp("t", "confirmed", &format!("Memory {i}")),
                )
            })
            .collect();
        let text = memory_notice(
            &Stamps::new(),
            &new,
            &BTreeSet::new(),
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1 + NOTICE_LINES + 1);
        assert_eq!(
            lines[NOTICE_LINES],
            format!("+ {:08} Memory {}", NOTICE_LINES - 1, NOTICE_LINES - 1)
        );
        assert_eq!(lines[NOTICE_LINES + 1], "and 3 more: call index");
        let exact: Stamps = new.into_iter().take(NOTICE_LINES).collect();
        assert!(
            !memory_notice(
                &Stamps::new(),
                &exact,
                &BTreeSet::new(),
                &BTreeSet::new(),
                true
            )
            .unwrap()
            .contains("more")
        );
    }

    // Where the state folder is: paths' tests.
    #[test]
    fn snapshots_have_a_safe_name() {
        let store = "store /home/u/.local/share/user-memory/memories.sqlite3";
        let key = &crate::setup::sha256(store.as_bytes())[..STORE_KEY_CHARS];
        assert_eq!(
            file_name("claude", "0b9c-41aa_x.1", store),
            Some(format!("claude-0b9c-41aa_x.1-{key}.json"))
        );
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()), "{key}");
        // Each store, or server, names a file of its own.
        let names: std::collections::BTreeSet<_> = [
            store,
            "store /tmp/dev/memories.sqlite3",
            "server https://memory.example.com/mcp",
        ]
        .into_iter()
        .map(|store| file_name("claude", "s", store))
        .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        let long = "x".repeat(SESSION_ID_CHARS + 1);
        for unsafe_id in [
            "",
            "../escape",
            "a/b",
            ".hidden",
            "with space",
            long.as_str(),
        ] {
            assert_eq!(file_name("codex", unsafe_id, store), None, "{unsafe_id:?}");
        }
        assert!(file_name("codex", &"x".repeat(SESSION_ID_CHARS), store).is_some());
    }

    #[test]
    fn snapshots_are_private_replaced_and_old_ones_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("state/user-memory/sessions");
        let path = sessions.join("claude-s1.json");
        let first = Snapshot {
            projects: Some(vec![Project {
                folders: Vec::new(),
                key: "github.com/acme/app".into(),
                source: THIS_FOLDER.into(),
            }]),
            stamps: stamps(&[("11111111-aaaa", stamp("t", "confirmed", "Prefer Rust"))]),
            changes: None,
        };
        let old = sessions.join("codex-gone.json");
        let recent = sessions.join("codex-recent.json");
        let other = sessions.join("notes.txt");
        // Temporary files of saves cut off by their hook's limit.
        let leftover = sessions.join(format!(".codex-cut.json.1{TEMPORARY}"));
        let in_flight = sessions.join(format!(".codex-now.json.2{TEMPORARY}"));
        save(&path, &first, &Landing::default()).unwrap();
        assert_eq!(load(&path).as_ref(), Some(&first));
        for private in [&path, &sessions] {
            assert!(
                private::is_private(private).unwrap(),
                "{}",
                private.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&sessions), 0o700);
        }
        const DAY: Duration = Duration::from_secs(24 * 60 * 60);
        for (file, age) in [
            (&old, DAY * 31),
            (&recent, DAY * 29),
            (&other, DAY * 31),
            (&leftover, LEFTOVER_AGE * 2),
            (&in_flight, Duration::ZERO),
        ] {
            fs::write(file, "{}").unwrap();
            fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(SystemTime::now() - age)
                .unwrap();
        }
        let second = Snapshot {
            projects: None,
            stamps: Stamps::new(),
            changes: None,
        };
        save(&path, &second, &Landing::default()).unwrap();
        assert_eq!(load(&path), Some(second));
        assert!(!old.exists(), "a snapshot over 30 days old is removed");
        assert!(recent.exists(), "a snapshot under 30 days old is kept");
        assert!(other.exists(), "only snapshots are removed");
        assert!(!leftover.exists(), "a cut-off save's file is removed");
        assert!(in_flight.exists(), "a save under way keeps its file");
        assert_eq!(
            fs::read_dir(&sessions).unwrap().count(),
            4,
            "no temporary file of this save left"
        );
    }

    /// A writer that keeps nothing and counts the calls made to it.
    #[derive(Default)]
    struct Calls(usize);

    impl io::Write for Calls {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += 1;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_snapshot_reaches_its_file_in_one_write() {
        // As large as a session's on a 10,000-memory store.
        let snapshot = Snapshot {
            projects: None,
            stamps: (0..10_000)
                .map(|i| {
                    (
                        format!("{i:08}-0000-4000-8000-000000000000"),
                        stamp(
                            "2026-10-01 10:00:00",
                            "confirmed",
                            "Six words of a title here",
                        ),
                    )
                })
                .collect(),
            changes: None,
        };
        let mut calls = Calls::default();
        write_whole(&mut calls, &snapshot).unwrap();
        assert_eq!(calls.0, 1);
    }

    #[test]
    fn an_abandoned_save_leaves_the_snapshot_and_a_landed_one_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions/claude-s.json");
        let told = Snapshot {
            projects: None,
            stamps: stamps(&[("11111111-aaaa", stamp("t", "confirmed", "Prefer Rust"))]),
            changes: None,
        };
        save(&path, &told, &Landing::default()).unwrap();
        // Abandoned before it replaced the file: the session keeps what it
        // was told, and no temporary file stays.
        let landing = Landing::default();
        assert!(!landing.abandon());
        let unsaid = Snapshot {
            projects: None,
            stamps: Stamps::new(),
            changes: None,
        };
        let err = save(&path, &unsaid, &landing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(load(&path).as_ref(), Some(&told));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        // Landed first: abandoning it says so, and the hook tells it.
        let landing = Landing::default();
        save(&path, &unsaid, &landing).unwrap();
        assert!(landing.abandon());
        assert_eq!(load(&path), Some(unsaid));
    }

    #[test]
    fn a_changed_project_set_opens_the_notice() {
        let project = |key: &str, source: &str| Project {
            folders: Vec::new(),
            key: key.into(),
            source: source.into(),
        };
        let kept = stamps(&[("11111111-aaaa", stamp("t", "confirmed", "Prefer Rust"))]);
        let old = Snapshot {
            projects: Some(vec![
                project("github.com/x/app", THIS_FOLDER),
                project("github.com/x/old", LISTED),
                project("github.com/x/tool", LINKED),
                project("local:app", PENDING),
            ]),
            stamps: kept.clone(),
            changes: None,
        };
        let mut gained = kept.clone();
        gained.insert(
            "22222222-bbbb".into(),
            stamp("t", "confirmed", "Run the web tests"),
        );
        let new = Snapshot {
            projects: Some(vec![
                project("github.com/x/app", THIS_FOLDER),
                project("github.com/x/web", LISTED),
            ]),
            stamps: gained,
            changes: None,
        };
        assert_eq!(
            notice(&old, &new, &BTreeSet::new(), &BTreeSet::new()).unwrap(),
            [
                "Projects changed: + github.com/x/web (listed in .user-memory); - github.com/x/old (no longer listed); - github.com/x/tool (unlinked); - local:app (moved)",
                HEADER,
                "+ 22222222 Run the web tests",
            ]
            .join("\n")
        );
        // A set change alone is news too; an unknown earlier set is not.
        let same_memories = Snapshot {
            projects: new.projects.clone(),
            stamps: kept.clone(),
            changes: None,
        };
        assert!(
            notice(&old, &same_memories, &BTreeSet::new(), &BTreeSet::new())
                .unwrap()
                .starts_with("Projects changed: ")
        );
        let unknown = Snapshot {
            projects: None,
            stamps: kept,
            changes: None,
        };
        assert_eq!(
            notice(&unknown, &same_memories, &BTreeSet::new(), &BTreeSet::new()),
            None
        );
        assert_eq!(notice(&old, &old, &BTreeSet::new(), &BTreeSet::new()), None);
    }

    fn seen_from(key: &str, source: &str, folders: &[&str]) -> Project {
        Project {
            key: key.into(),
            source: source.into(),
            folders: folders.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    #[test]
    fn a_project_stays_until_no_folder_that_gave_it_gives_it() {
        let before = [
            seen_from("a", LISTED, &["/w", "/w/a"]),
            seen_from("b", LISTED, &["/w"]),
            seen_from("link", LINKED, &[]),
            seen_from("old", LISTED, &[]),
        ];
        // In /w/a: b, seen only from /w, stays; a gains nothing new; the
        // link, gone from the server, and a project from an older snapshot
        // with no folders leave.
        let now = kept(
            vec![seen_from("a", THIS_FOLDER, &[])],
            Some(&before),
            "/w/a",
        );
        assert_eq!(
            now,
            [
                seen_from("a", THIS_FOLDER, &["/w/a", "/w"]),
                seen_from("b", LISTED, &["/w"]),
            ]
        );
        // Back in /w, which no longer lists b: b leaves; a, also seen from
        // /w/a, stays though /w no longer lists it either.
        let now = kept(Vec::new(), Some(&now), "/w");
        assert_eq!(now, [seen_from("a", THIS_FOLDER, &["/w/a"])]);
        // A link keeps no folder, and a first look has nothing before.
        let now = kept(vec![seen_from("link", LINKED, &[])], None, "/w");
        assert_eq!(now, [seen_from("link", LINKED, &[])]);
    }
}
