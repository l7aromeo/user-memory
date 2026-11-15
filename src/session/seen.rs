//! The standing rules session start last showed for one store on this
//! machine, so the next session start lists those added or changed since.
//! One file per store, `seen-<store key>.json` beside the session
//! snapshots, holding ids, prints and project keys only: never a memory's
//! text or origin.
//!
//! It is compared by id and print, never by time: imported memories keep
//! their old dates, and a server's clock may differ from this machine's.
//! A rule this file holds with the same print is never listed again,
//! wherever it now lives, so a move or a rekey lists nothing; one it has
//! never held is listed when it comes into view. A project the file has
//! never covered is taken silently, rules and all, since nothing but a time
//! could tell its old rules from new ones.

use crate::{memory::StampsReply, private};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

/// This file's layout; a file in another layout is read as none, so the
/// session after an upgrade that changes it lists nothing, then starts
/// afresh.
const FORMAT: u32 = 1;

/// Files unused this long belong to a store this machine no longer uses:
/// every start that takes its store's turn marks that store's files used,
/// and removes other stores' files unused this long. Far longer than the
/// snapshots' 30 days: a session after a month away still lists the rules
/// added meanwhile.
pub(super) const SEEN_AGE: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Opens the name of every file of this module, so the clean-up tells them
/// from session snapshots.
const PREFIX: &str = "seen-";

/// How often a start waiting for another start's turn at the file tries
/// again.
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// The group of a store's global memories; every other group is a
/// project's, by its key.
const GLOBAL: &str = "";

/// Whether `name`, a file in the sessions folder, is one of this module's.
pub(super) fn is_seen_file(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// The file of the store named `store`.
pub(super) fn path(store: &str) -> Option<PathBuf> {
    Some(
        super::live::sessions_dir()?
            .join(format!("{PREFIX}{}.json", super::live::store_key(store))),
    )
}

/// What the file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Seen {
    format: u32,
    /// The way the store printed the rules held here.
    print: u32,
    /// Each group the file covers, global as `""` and each project by its
    /// key.
    groups: BTreeMap<String, Group>,
    /// Rules listed to an agent at a start that could not tell the user,
    /// which the next `startup` tells.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    untold: BTreeSet<String>,
}

/// One group's rules as last shown, and its agents' memories as of the
/// user's last message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Group {
    /// Each rule's print, by id.
    #[serde(default)]
    rules: BTreeMap<String, String>,
    #[serde(default)]
    agents: BTreeSet<String>,
}

/// What session start sees now: the stamps reply with prints, and the
/// projects it covers besides the global memories.
pub(super) struct View<'a> {
    pub reply: &'a StampsReply,
    pub projects: &'a [String],
}

impl View<'_> {
    /// The groups this view covers.
    fn covered(&self) -> impl Iterator<Item = &str> {
        std::iter::once(GLOBAL).chain(self.projects.iter().map(String::as_str))
    }

    /// The rules and agents' memories of the group `key`, as a group.
    fn group(&self, key: &str) -> Group {
        Group {
            rules: self
                .reply
                .rules
                .iter()
                .filter(|(_, rule)| rule.project == key)
                .map(|(id, rule)| (id.clone(), rule.print.clone()))
                .collect(),
            agents: self
                .reply
                .agents
                .iter()
                .filter(|(_, project)| *project == key)
                .map(|(id, _)| id.clone())
                .collect(),
        }
    }
}

/// What the comparison found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Found {
    /// The rules to list, by id.
    pub rules: Vec<String>,
    /// What to tell the user, at a `startup` with a rule to tell.
    pub told: Option<Told>,
}

/// The user's message: rules added or changed, and agents' memories
/// recorded, since the user was last told.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Told {
    pub rules: usize,
    pub agents: usize,
}

/// Compares what the file held, `before`, with `view`, at a start that can
/// tell the user when `startup`: what to list and tell, and what the file
/// then holds. With no file, or one in another layout or print, nothing is
/// listed and the file starts afresh.
pub(super) fn compare(before: Option<&Seen>, view: &View, startup: bool) -> (Found, Seen) {
    let mut after = Seen {
        format: FORMAT,
        print: crate::memory::PRINT_FORMAT,
        ..Seen::default()
    };
    let before = before.filter(|b| b.format == FORMAT && b.print == crate::memory::PRINT_FORMAT);
    let Some(before) = before else {
        for key in view.covered() {
            after.groups.insert(key.to_owned(), view.group(key));
        }
        return (Found::default(), after);
    };
    let held = |id: &str, print: &str| {
        before
            .groups
            .values()
            .any(|group| group.rules.get(id).is_some_and(|held| held == print))
    };
    let new: Vec<String> = view
        .reply
        .rules
        .iter()
        .filter(|(id, rule)| {
            (rule.project == GLOBAL || before.groups.contains_key(&rule.project))
                && !held(id, &rule.print)
        })
        .map(|(id, _)| id.clone())
        .collect();
    let told_rules: BTreeSet<&String> = if startup {
        new.iter()
            .chain(
                before
                    .untold
                    .iter()
                    .filter(|id| view.reply.rules.contains_key(*id)),
            )
            .collect()
    } else {
        BTreeSet::new()
    };
    let telling = !told_rules.is_empty();
    let held_agents = |id: &str| before.groups.values().any(|g| g.agents.contains(id));
    let mut new_agents = 0;
    after.groups = before.groups.clone();
    for key in view.covered() {
        let now = view.group(key);
        let group = match before.groups.get(key) {
            Some(was) => {
                new_agents += now.agents.iter().filter(|id| !held_agents(id)).count();
                Group {
                    rules: now.rules,
                    agents: if telling {
                        now.agents
                    } else {
                        was.agents.clone()
                    },
                }
            }
            None => now,
        };
        after.groups.insert(key.to_owned(), group);
    }
    let still_rules: BTreeSet<&String> =
        after.groups.values().flat_map(|g| g.rules.keys()).collect();
    after.untold = before
        .untold
        .iter()
        .chain(new.iter().filter(|_| !startup))
        .filter(|id| !told_rules.contains(id) && still_rules.contains(id))
        .cloned()
        .collect();
    let told = telling.then_some(Told {
        rules: told_rules.len(),
        agents: new_agents,
    });
    (Found { rules: new, told }, after)
}

/// The file at `path`, when it is there and readable.
fn load(path: &Path) -> Option<Seen> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Compares the file at `path` with `view` and saves what it then holds,
/// holding the store's turn at the file meanwhile, so two starts at once
/// each list a rule at least once and keep each other's projects. Without
/// the turn by `deadline`, or in a folder it cannot write, the start still
/// lists what is new and saves nothing: a rule may be listed twice, never
/// lost. A file unchanged is not written.
pub(super) fn update(path: &Path, view: &View, startup: bool, deadline: Instant) -> Found {
    let turn = take_turn(path, deadline);
    let before = load(path);
    let (found, after) = compare(before.as_ref(), view, startup);
    if let Some(lock) = turn {
        let now = SystemTime::now();
        if before.as_ref() == Some(&after) {
            // Unchanged, so not written: marked used all the same.
            let _ = fs::File::options()
                .write(true)
                .open(path)
                .and_then(|file| file.set_modified(now));
        } else {
            let _ = save(path, &after);
        }
        let _ = lock.set_modified(now);
        remove_unused(path, now);
    }
    found
}

/// Removes the files of other stores than the one at `own` in its folder
/// unused for [`SEEN_AGE`] before `now`.
fn remove_unused(own: &Path, now: SystemTime) {
    let (Some(dir), Some(stem)) = (own.parent(), own.file_stem()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if !is_seen_file(&name.to_string_lossy()) || path.file_stem() == Some(stem) {
            continue;
        }
        let unused = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SEEN_AGE);
        if unused {
            let _ = fs::remove_file(path);
        }
    }
}

/// This store's turn at its file, held until the returned file is dropped;
/// `None` when it can't be had by `deadline`.
fn take_turn(path: &Path, deadline: Instant) -> Option<fs::File> {
    let dir = path.parent()?;
    private::create_dir(dir).ok()?;
    let lock = private::create_file(
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false),
        &path.with_extension("lock"),
    )
    .ok()?;
    loop {
        match lock.try_lock() {
            Ok(()) => return Some(lock),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(_) => return None,
        }
    }
}

/// Writes `seen` to `path` whole: a temporary file renamed over it, so a
/// start cut off leaves the old file or the new one, never part of one.
fn save(path: &Path, seen: &Seen) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = path
        .file_name()
        .ok_or(io::ErrorKind::InvalidInput)?
        .to_string_lossy();
    let temporary = dir.join(format!(".{name}.{}.partial", std::process::id()));
    let written = (|| {
        let mut file = private::create_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            &temporary,
        )?;
        io::Write::write_all(&mut file, &serde_json::to_vec(seen)?)?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Rule;

    const P: &str = "github.com/x/p";
    const Q: &str = "github.com/x/q";

    /// A reply with these rules (id, print, project) and agents' memories
    /// (id, project).
    fn reply(rules: &[(&str, &str, &str)], agents: &[(&str, &str)]) -> StampsReply {
        StampsReply {
            prints: true,
            rules: rules
                .iter()
                .map(|(id, print, project)| {
                    (
                        (*id).to_owned(),
                        Rule {
                            print: (*print).to_owned(),
                            client: String::new(),
                            origin: String::new(),
                            project: (*project).to_owned(),
                        },
                    )
                })
                .collect(),
            agents: agents
                .iter()
                .map(|(id, project)| ((*id).to_owned(), (*project).to_owned()))
                .collect(),
            ..StampsReply::default()
        }
    }

    /// Runs a start that covers `projects` against `state`, keeping what the
    /// file then holds: the rules listed and the user's message.
    fn start(
        state: &mut Option<Seen>,
        reply: &StampsReply,
        projects: &[&str],
        startup: bool,
    ) -> (Vec<String>, Option<(usize, usize)>) {
        let projects: Vec<String> = projects.iter().map(|p| (*p).to_owned()).collect();
        let view = View {
            reply,
            projects: &projects,
        };
        let (found, after) = compare(state.as_ref(), &view, startup);
        *state = Some(after);
        (
            found.rules,
            found.told.map(|told| (told.rules, told.agents)),
        )
    }

    #[test]
    fn a_rule_is_listed_once_and_the_first_start_lists_nothing() {
        let mut state = None;
        let old = reply(&[("a", "1", "")], &[]);
        assert_eq!(start(&mut state, &old, &[], true), (vec![], None));
        let added = reply(&[("a", "1", ""), ("b", "1", "")], &[]);
        assert_eq!(
            start(&mut state, &added, &[], true),
            (vec!["b".to_owned()], Some((1, 0)))
        );
        assert_eq!(start(&mut state, &added, &[], true), (vec![], None));
        // Changed: listed again, once.
        let changed = reply(&[("a", "1", ""), ("b", "2", "")], &[]);
        assert_eq!(start(&mut state, &changed, &[], true).0, ["b"]);
        assert_eq!(
            start(&mut state, &changed, &[], true).0,
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_project_first_covered_is_taken_silently_then_compared() {
        let mut state = None;
        start(&mut state, &reply(&[], &[]), &[], true);
        let first = reply(&[("p1", "1", P)], &[("o1", P)]);
        assert_eq!(start(&mut state, &first, &[P], true), (vec![], None));
        let later = reply(&[("p1", "1", P), ("p2", "1", P)], &[("o1", P), ("o2", P)]);
        assert_eq!(
            start(&mut state, &later, &[P], true),
            (vec!["p2".to_owned()], Some((1, 1)))
        );
    }

    #[test]
    fn a_move_lists_a_rule_only_if_the_file_never_held_it() {
        let mut state = None;
        start(&mut state, &reply(&[("m", "1", P)], &[]), &[P, Q], true);
        // Between two covered projects, or a rekey to one never covered:
        // nothing.
        assert_eq!(
            start(&mut state, &reply(&[("m", "1", Q)], &[]), &[P, Q], true).0,
            Vec::<String>::new()
        );
        assert_eq!(
            start(
                &mut state,
                &reply(&[("m", "1", "github.com/x/r")], &[]),
                &["github.com/x/r"],
                true
            )
            .0,
            Vec::<String>::new()
        );
        // Recorded where this machine never looks, then moved into view:
        // listed.
        assert_eq!(
            start(&mut state, &reply(&[("n", "1", P)], &[]), &[P, Q], true).0,
            ["n"]
        );
        // Widened to global: a new print, listed.
        assert_eq!(
            start(&mut state, &reply(&[("n", "2", "")], &[]), &[P, Q], true).0,
            ["n"]
        );
    }

    #[test]
    fn a_rule_listed_where_the_user_could_not_be_told_is_told_at_the_next_startup() {
        let mut state = None;
        start(&mut state, &reply(&[], &[("o1", "")]), &[], true);
        let added = reply(&[("r", "1", "")], &[("o1", ""), ("o2", "")]);
        // A compact start lists it to the agent, telling no one.
        assert_eq!(
            start(&mut state, &added, &[], false),
            (vec!["r".to_owned()], None)
        );
        // The next startup lists nothing new but tells the user of it.
        assert_eq!(start(&mut state, &added, &[], true), (vec![], Some((1, 1))));
        assert_eq!(start(&mut state, &added, &[], true), (vec![], None));
    }

    #[test]
    fn agents_memories_alone_tell_nothing_and_are_counted_with_the_next_rule() {
        let mut state = None;
        start(&mut state, &reply(&[], &[]), &[], true);
        let agents = reply(&[], &[("o1", ""), ("o2", "")]);
        assert_eq!(start(&mut state, &agents, &[], true), (vec![], None));
        let rule = reply(&[("r", "1", "")], &[("o1", ""), ("o2", ""), ("o3", "")]);
        assert_eq!(
            start(&mut state, &rule, &[], true),
            (vec!["r".to_owned()], Some((1, 3)))
        );
    }

    #[test]
    fn a_file_of_another_layout_or_print_starts_afresh() {
        let view_reply = reply(&[("a", "1", "")], &[]);
        for (format, print) in [
            (FORMAT + 1, crate::memory::PRINT_FORMAT),
            (FORMAT, crate::memory::PRINT_FORMAT + 1),
        ] {
            let mut state = Some(Seen {
                format,
                print,
                ..Seen::default()
            });
            assert_eq!(start(&mut state, &view_reply, &[], true), (vec![], None));
            assert_eq!(start(&mut state, &view_reply, &[], true), (vec![], None));
        }
    }

    #[test]
    fn a_start_keeps_the_projects_it_does_not_cover() {
        let mut state = None;
        start(&mut state, &reply(&[("p1", "1", P)], &[]), &[P], true);
        start(&mut state, &reply(&[("q1", "1", Q)], &[]), &[Q], true);
        let held = state.unwrap();
        assert_eq!(
            held.groups.keys().map(String::as_str).collect::<Vec<_>>(),
            ["", P, Q]
        );
        assert!(held.groups[P].rules.contains_key("p1"));
    }

    #[test]
    fn two_starts_at_once_each_list_a_rule_at_least_once_and_keep_both_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen-0000.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = reply(&[], &[]);
        let projects: Vec<String> = vec![P.into(), Q.into()];
        update(
            &path,
            &View {
                reply: &first,
                projects: &projects,
            },
            true,
            deadline,
        );
        let added = reply(&[("r", "1", "")], &[]);
        let listed: Vec<Vec<String>> = std::thread::scope(|scope| {
            let runs: Vec<_> = [P, Q]
                .into_iter()
                .map(|project| {
                    let added = &added;
                    let path = &path;
                    scope.spawn(move || {
                        let projects = vec![project.to_owned()];
                        update(
                            path,
                            &View {
                                reply: added,
                                projects: &projects,
                            },
                            true,
                            deadline,
                        )
                        .rules
                    })
                })
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        assert!(listed.iter().any(|rules| rules == &["r"]), "{listed:?}");
        let held = load(&path).unwrap();
        assert!(held.groups.contains_key(P) && held.groups.contains_key(Q));
        // Neither left its temporary file or lost the lock for the next.
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".partial")), "{names:?}");
        assert_eq!(
            update(
                &path,
                &View {
                    reply: &added,
                    projects: &projects
                },
                true,
                deadline
            )
            .rules,
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_file_left_unreadable_or_in_part_is_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen-0000.json");
        fs::write(&path, b"{\"format\":1,\"print\":1,\"gro").unwrap();
        assert_eq!(load(&path), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_it_cannot_write_still_lists_and_writes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen-0000.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        let projects: Vec<String> = Vec::new();
        update(
            &path,
            &View {
                reply: &reply(&[], &[]),
                projects: &projects,
            },
            true,
            deadline,
        );
        let before = fs::read(&path).unwrap();
        fs::remove_file(path.with_extension("lock")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let added = reply(&[("r", "1", "")], &[]);
        let found = update(
            &path,
            &View {
                reply: &added,
                projects: &projects,
            },
            true,
            deadline,
        );
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(found.rules, ["r"]);
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
