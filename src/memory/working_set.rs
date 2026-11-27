//! What a compacted context worked with (#118), printed again after the
//! compaction from the store as it is now: the memories it read, recorded,
//! was held back by and was pushed, each kind newest first, then those a
//! search found, as pointers. A memory changed since prints its current
//! text, a superseded one the memory in force that replaced it, and a
//! forgotten or stale one nothing, so a changed rule never comes back in
//! its old form.

use super::{FitUnit, Memory, ids::SHORT_ID, push::label, read::passage, schema::get};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;
use uuid::Uuid;

/// The longest memory printed whole: about the 95th percentile of a rule's
/// text, counted on synthetic rules as `PROJECT_RULE_FLOOR` was (a median
/// 187 characters, 242 at the 90th). A longer one prints its title and
/// the passages the session was shown.
pub(crate) const WORKING_SET_WHOLE_CHARS: usize = 600;

/// The most superseding memories followed from one id: a chain longer
/// than this is a loop, never a history, and restores nothing.
const SUPERSEDE_HOPS: usize = 8;

/// Opens the working set.
const WORKING_SET_HEADER: &str = "What this context worked with before it was compacted, from user-memory as it is now (a memory changed since shows its current text):";

/// Ends a line the working set names without its text.
const READ_AGAIN: &str = "read it again before acting on it";

/// What the context did with a memory, in the order the working set
/// prints them: what it chose to read comes before what a hook guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Use {
    Read,
    Recorded,
    Held,
    Pushed,
    Found,
}

impl Use {
    /// The line that opens this kind's memories.
    fn heading(self) -> &'static str {
        match self {
            Self::Read => "Read:",
            Self::Recorded => "Recorded or changed:",
            Self::Held => "Shown before a command ran:",
            Self::Pushed => "Shown after a step:",
            Self::Found => "Found by a search:",
        }
    }
}

/// One memory of the working set, as the session's state keeps it: its
/// full id, its strongest use, and the passages a push showed of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) how: Use,
    pub(crate) passages: Vec<i64>,
}

/// A memory the working set printed, by the id now in force, for the
/// entry `from`: whole, or as the `passages` it showed, or (neither) as a
/// pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Printed {
    pub(crate) from: String,
    pub(crate) id: String,
    pub(crate) whole: bool,
    pub(crate) passages: Vec<i64>,
}

/// The working set's text and what it printed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Restored {
    pub(crate) text: String,
    pub(crate) printed: Vec<Printed>,
}

/// The working set of `entries`, grouped by use and in their order within
/// each, within `room` in `unit`: each memory whole when it is at most
/// [`WORKING_SET_WHOLE_CHARS`], else its title and the passages shown,
/// else a pointer; found ones are pointers. Read through the store `conn`,
/// or without it (a server's store) as pointers by id. Nothing for no
/// memory in force.
pub(crate) fn restore(
    conn: Option<&Connection>,
    entries: &[Entry],
    room: usize,
    unit: FitUnit,
) -> Result<Restored> {
    let mut memories: Vec<(Use, Vec<(Printed, String)>)> = Vec::new();
    let mut seen = HashSet::new();
    let mut ordered: Vec<&Entry> = entries.iter().collect();
    ordered.sort_by_key(|e| e.how);
    for entry in ordered {
        let Some(conn) = conn else {
            if seen.insert(entry.id.clone()) {
                let line = format!("- {} {READ_AGAIN}", short(&entry.id));
                memories.push((entry.how, vec![(pointer(&entry.id, &entry.id), line)]));
            }
            continue;
        };
        let Some(memory) = in_force(conn, &entry.id)? else {
            continue;
        };
        let id = memory.id.to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some((_, label)) = label(
            &memory.source,
            &memory.status,
            &memory.memory_type,
            memory.origins.len(),
        ) else {
            continue;
        };
        let short = short(&id);
        let title = one_line(&memory.title);
        // Its forms, the fullest first: what the room allows prints.
        let mut forms = Vec::new();
        if entry.how != Use::Found {
            let content = one_line(&memory.content);
            if content.chars().count() <= WORKING_SET_WHOLE_CHARS {
                let whole = Printed {
                    from: entry.id.clone(),
                    id: id.clone(),
                    whole: true,
                    passages: Vec::new(),
                };
                forms.push((whole, format!("- {short} {label}: {content}")));
            }
            let shown: Vec<(i64, String)> = entry
                .passages
                .iter()
                .map(|&slot| (slot, one_line(passage(&memory, slot).0)))
                .filter(|(_, text)| !text.is_empty())
                .collect();
            if !shown.is_empty() {
                let texts: Vec<&str> = shown.iter().map(|(_, t)| t.as_str()).collect();
                let part = Printed {
                    from: entry.id.clone(),
                    id: id.clone(),
                    whole: false,
                    passages: shown.iter().map(|(slot, _)| *slot).collect(),
                };
                let line = format!("- {short} {label}: {title}; shown: {} …", texts.join(" … "));
                forms.push((part, line));
            }
        }
        forms.push((
            pointer(&entry.id, &id),
            format!("- {short} {title}; {READ_AGAIN}"),
        ));
        memories.push((entry.how, forms));
    }
    Ok(fit(memories, room, unit))
}

/// The text of `memories`, each by its first form that fits in `room`,
/// its kind's heading before the first of its kind; a memory not even a
/// pointer fits for is left out. Nothing when none fits.
fn fit(memories: Vec<(Use, Vec<(Printed, String)>)>, room: usize, unit: FitUnit) -> Restored {
    let mut restored = Restored::default();
    let mut text = WORKING_SET_HEADER.to_owned();
    let mut used = unit.len(&text);
    let mut heading: Option<Use> = None;
    for (how, forms) in memories {
        let head = (heading != Some(how)).then(|| how.heading());
        let head_len = head.map_or(0, |h| unit.len(h) + 1);
        let fits = forms
            .into_iter()
            .find(|(_, line)| used + head_len + unit.len(line) < room);
        let Some((printed, line)) = fits else {
            continue;
        };
        if let Some(head) = head {
            text.push('\n');
            text.push_str(head);
            heading = Some(how);
        }
        text.push('\n');
        text.push_str(&line);
        used += head_len + unit.len(&line) + 1;
        restored.printed.push(printed);
    }
    if !restored.printed.is_empty() {
        restored.text = text;
    }
    restored
}

/// The memory in force that `id` stands for now: itself while it is
/// confirmed or tentative, the memory that superseded it when that one is,
/// following at most [`SUPERSEDE_HOPS`]; none when forgotten or stale.
fn in_force(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let Ok(mut id) = Uuid::parse_str(id) else {
        return Ok(None);
    };
    for _ in 0..=SUPERSEDE_HOPS {
        let Some(memory) = get(conn, id)? else {
            return Ok(None);
        };
        match (memory.status.as_str(), memory.superseded_by) {
            ("confirmed" | "tentative", _) => return Ok(Some(memory)),
            ("superseded", Some(next)) => id = next,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// A pointer's record of `id`, for the entry `from`: printed by id, no
/// passage.
fn pointer(from: &str, id: &str) -> Printed {
    Printed {
        from: from.to_owned(),
        id: id.to_owned(),
        whole: false,
        passages: Vec::new(),
    }
}

/// The id as the index prints it.
fn short(id: &str) -> &str {
    id.get(..SHORT_ID).unwrap_or(id)
}

/// `text` on one line, its runs of white space one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
