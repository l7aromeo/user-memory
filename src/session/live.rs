//! What a running session last saw of memory, and the notice that tells it
//! what changed since. Each session's snapshot is a file under the user's
//! state directory, written at session start and after each prompt.

use crate::{
    memory::{Stamp, Stamps},
    paths, private,
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Changes listed in one notice; the rest are counted, as the index lists them.
const NOTICE_LINES: usize = 10;
/// Snapshots untouched this long belong to sessions long over; writing any
/// snapshot removes them.
const SNAPSHOT_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Session ids are short; a longer one is not a client's and is not used in
/// a file name.
const SESSION_ID_CHARS: usize = 128;
/// Opens the notice; the agent reads a changed memory before applying it.
const HEADER: &str = "Memory changed since this session last looked (+ new, ~ changed, - no longer active; read before applying):";

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
}

/// What a session saw: its projects, unknown in a snapshot saved before
/// they were, and its memories.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub projects: Option<Vec<Project>>,
    pub stamps: Stamps,
}

/// A snapshot file as this or an earlier version wrote it.
#[derive(Deserialize)]
#[serde(untagged)]
enum Saved {
    Current(Snapshot),
    StampsOnly(Stamps),
}

/// The snapshot's file name for `client`'s session `session_id`, or `None`
/// when the id could not safely name a file.
fn file_name(client: &str, session_id: &str) -> Option<String> {
    let safe = !session_id.is_empty()
        && session_id.len() <= SESSION_ID_CHARS
        && !session_id.starts_with('.')
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    safe.then(|| format!("{client}-{session_id}.json"))
}

/// The snapshot file of `client`'s session `session_id`.
pub(super) fn snapshot_path(client: &str, session_id: &str) -> Option<PathBuf> {
    let dir = paths::state_dir()?.join("sessions");
    Some(dir.join(file_name(client, session_id)?))
}

/// The snapshot saved at `path`, if one is there and readable; one saved
/// before snapshots held projects loads with them unknown.
pub(super) fn load(path: &Path) -> Option<Snapshot> {
    match serde_json::from_slice(&fs::read(path).ok()?).ok()? {
        Saved::Current(snapshot) => Some(snapshot),
        Saved::StampsOnly(stamps) => Some(Snapshot {
            projects: None,
            stamps,
        }),
    }
}

/// Saves `snapshot` at `path` (folders and file private, replaced whole),
/// then removes the snapshots of sessions over [`SNAPSHOT_AGE`] ago.
pub(super) fn save(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    private::create_dir(dir)?;
    let name = path
        .file_name()
        .ok_or(io::ErrorKind::InvalidInput)?
        .to_string_lossy();
    let temporary = dir.join(format!(".{name}.{}", std::process::id()));
    let written = (|| {
        let file = private::create_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            &temporary,
        )?;
        serde_json::to_writer(file, snapshot)?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written?;
    remove_old(dir, SystemTime::now());
    Ok(())
}

/// Removes the snapshots in `dir` last written over [`SNAPSHOT_AGE`] before
/// `now`.
fn remove_old(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SNAPSHOT_AGE);
        if old && path.extension().is_some_and(|e| e == "json") {
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
    (!parts.is_empty()).then(|| format!("Projects changed: {}", parts.join("; ")))
}

/// The notice for a session that last saw `old` and now sees `new`, or
/// `None` when nothing changed: the projects it gained and lost, then
/// memories added, changed and no longer active, the first
/// [`NOTICE_LINES`] listed and the rest counted.
pub(super) fn notice(old: &Snapshot, new: &Snapshot) -> Option<String> {
    let projects = projects_changed(old.projects.as_deref(), new.projects.as_deref());
    let memories = memory_notice(&old.stamps, &new.stamps);
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
fn memory_notice(old: &Stamps, new: &Stamps) -> Option<String> {
    let short = |id: &str| id.chars().take(8).collect::<String>();
    let line =
        |sign: char, id: &str, stamp: &Stamp| format!("{sign} {} {}", short(id), stamp.title);
    let added = new
        .iter()
        .filter(|(id, _)| !old.contains_key(*id))
        .map(|(id, stamp)| line('+', id, stamp));
    let changed = new
        .iter()
        .filter(|(id, stamp)| old.get(*id).is_some_and(|was| was != *stamp))
        .map(|(id, stamp)| line('~', id, stamp));
    let removed = old
        .keys()
        .filter(|id| !new.contains_key(*id))
        .map(|id| format!("- {}", short(id)));
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
            updated_at: updated_at.into(),
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
        assert_eq!(memory_notice(&old, &old), None);
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
            memory_notice(&old, &new).unwrap(),
            [
                HEADER,
                "+ 55555555 Ask before pushing",
                "~ 22222222 Likes tea",
                "~ 44444444 New title",
                "- 33333333",
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
        let text = memory_notice(&Stamps::new(), &new).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1 + NOTICE_LINES + 1);
        assert_eq!(
            lines[NOTICE_LINES],
            format!("+ {:08} Memory {}", NOTICE_LINES - 1, NOTICE_LINES - 1)
        );
        assert_eq!(lines[NOTICE_LINES + 1], "and 3 more: call index");
        let exact: Stamps = new.into_iter().take(NOTICE_LINES).collect();
        assert!(
            !memory_notice(&Stamps::new(), &exact)
                .unwrap()
                .contains("more")
        );
    }

    // Where the state folder is: paths' tests.
    #[test]
    fn snapshots_have_a_safe_name() {
        assert_eq!(
            file_name("claude", "0b9c-41aa_x.1").as_deref(),
            Some("claude-0b9c-41aa_x.1.json")
        );
        let long = "x".repeat(SESSION_ID_CHARS + 1);
        for unsafe_id in [
            "",
            "../escape",
            "a/b",
            ".hidden",
            "with space",
            long.as_str(),
        ] {
            assert_eq!(file_name("codex", unsafe_id), None, "{unsafe_id:?}");
        }
        assert!(file_name("codex", &"x".repeat(SESSION_ID_CHARS)).is_some());
    }

    #[test]
    fn snapshots_are_private_replaced_and_old_ones_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("state/user-memory/sessions");
        let path = sessions.join("claude-s1.json");
        let first = Snapshot {
            projects: Some(vec![Project {
                key: "github.com/acme/app".into(),
                source: THIS_FOLDER.into(),
            }]),
            stamps: stamps(&[("11111111-aaaa", stamp("t", "confirmed", "Prefer Rust"))]),
        };
        let old = sessions.join("codex-gone.json");
        let recent = sessions.join("codex-recent.json");
        let other = sessions.join("notes.txt");
        save(&path, &first).unwrap();
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
        for (file, days) in [(&old, 31), (&recent, 29), (&other, 31)] {
            fs::write(file, "{}").unwrap();
            fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(SystemTime::now() - DAY * days)
                .unwrap();
        }
        let second = Snapshot {
            projects: None,
            stamps: Stamps::new(),
        };
        save(&path, &second).unwrap();
        assert_eq!(load(&path), Some(second));
        assert!(!old.exists(), "a snapshot over 30 days old is removed");
        assert!(recent.exists(), "a snapshot under 30 days old is kept");
        assert!(other.exists(), "only snapshots are removed");
        assert_eq!(
            fs::read_dir(&sessions).unwrap().count(),
            3,
            "no temporary file left"
        );
    }

    #[test]
    fn a_snapshot_from_before_projects_loads_with_them_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude-old.json");
        let saved = stamps(&[("11111111-aaaa", stamp("t", "confirmed", "Prefer Rust"))]);
        fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(
            load(&path),
            Some(Snapshot {
                projects: None,
                stamps: saved
            })
        );
    }

    #[test]
    fn a_changed_project_set_opens_the_notice() {
        let project = |key: &str, source: &str| Project {
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
        };
        assert_eq!(
            notice(&old, &new).unwrap(),
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
        };
        assert!(
            notice(&old, &same_memories)
                .unwrap()
                .starts_with("Projects changed: ")
        );
        let unknown = Snapshot {
            projects: None,
            stamps: kept,
        };
        assert_eq!(notice(&unknown, &same_memories), None);
        assert_eq!(notice(&old, &old), None);
    }
}
