//! What a hook pushes after an action (#117): the passages that pass the
//! matcher's bars, each on one line with the memory's id and a label of
//! whose word it is, under a header that says to read the memory before
//! applying it.
//!
//! The text comes from the store, read after the match: the delivery
//! index holds no text and no title. Where the store can't be read in
//! time, or lives on a server, a line names the id alone ([`POINTER`]).

use super::{
    delivery::{Error, View},
    ids::SHORT_ID,
    matcher::{self, Matched},
    read::is_rule,
    reads_as_wish,
};
use crate::memory::in_force::live;
use rusqlite::{Connection, OptionalExtension};
use std::time::Instant;

/// The most passages pushed after one action. More than three pass in 7 of
/// 666 bench actions, and a cap of three keeps 802 of 806 deliveries and
/// all recall (#117).
pub(crate) const PUSH_PER_EVENT: usize = 3;

/// The most characters one push holds, its header included: three whole
/// passages of 400 characters with their ids and labels. The bench's
/// largest push at three per event is 695 characters at 10,000 memories,
/// so it binds only on long passages with long labels.
pub(crate) const RECALL_CHARS: usize = 1_500;

/// The most passing matches a hook reads the text of: twice
/// [`PUSH_PER_EVENT`], so when the strongest were shown already in this
/// stretch the next ones can take their place, while a common term that
/// passes many never makes a hook read them all.
pub(crate) const READ_PER_EVENT: usize = 2 * PUSH_PER_EVENT;

/// Opens a push after an action.
const HEADER: &str =
    "From user-memory, matching this step (read one for the rest before applying it):";

/// Opens a push after an action that failed.
const FAILED_HEADER: &str =
    "From user-memory, matching this failed step (read one for the rest before applying it):";

/// What a line says of a memory whose text the hook did not read: a
/// server's store, or a store read past the hook's limit. It has no label:
/// whose word a memory is comes from its source, status and type, which
/// the delivery index does not hold, and a line that claims none sends the
/// agent to `read`, which gives them.
pub(crate) const POINTER: &str = "matches this step; read it before applying it";

/// Ends a passage that is part of its memory, so the agent knows there is
/// more to read.
const MORE: &str = " …";

/// One memory's line in a push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    /// The memory's id.
    pub(crate) id: String,
    /// The passage's slot, the key it is shown once by.
    pub(crate) passage: i64,
    /// The line as pushed, without its line break.
    pub(crate) text: String,
}

/// Whether `matched` passes a bar after its action: the failure bars when
/// the action failed, the push bars otherwise.
pub(crate) fn passes(matched: &Matched, failed: bool) -> bool {
    if failed {
        matched.failure()
    } else {
        matched.push()
    }
}

/// The lines for `passing`, matches of `view` strongest first: each
/// memory's passage and label from the store `conn`, read until
/// `deadline`, or the [`POINTER`] line without it. A memory gone from the
/// store, or whose passage no longer falls on its text, has no line.
pub(crate) fn lines(
    conn: Option<&Connection>,
    view: &View,
    passing: &[&Matched],
    deadline: Instant,
) -> Result<Vec<Line>, Error> {
    let mut lines = Vec::with_capacity(passing.len());
    for matched in passing {
        let id = view.id(matched.memory)?.to_owned();
        let short = id.get(..SHORT_ID).unwrap_or(&id);
        let text = match conn.filter(|_| Instant::now() < deadline) {
            Some(conn) => match labelled(conn, &id, matched)? {
                Some(Labelled { label, passage, .. }) => format!("- {short} {label}: {passage}"),
                None => continue,
            },
            None => format!("- {short} {POINTER}"),
        };
        lines.push(Line {
            id,
            passage: matched.slot,
            text,
        });
    }
    Ok(lines)
}

/// The push of `lines`, after an action that `failed` or not.
pub(crate) fn text<'a>(lines: impl IntoIterator<Item = &'a Line>, failed: bool) -> String {
    let mut text = if failed { FAILED_HEADER } else { HEADER }.to_owned();
    for line in lines {
        text.push('\n');
        text.push_str(&line.text);
    }
    text
}

/// The characters [`text`] adds to a push for its header.
pub(crate) fn header_chars(failed: bool) -> usize {
    if failed { FAILED_HEADER } else { HEADER }.chars().count()
}

/// Whose word a memory is, as its label says: what a hold may show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Voice {
    /// The user's rule.
    Rule,
    /// Any other memory the user stated: a decision, a fact.
    User,
    /// An agent's note about the work: a fact, decision, goal or lesson.
    Note,
    /// An agent's note that reads as the user's wish: an observation about
    /// the user, which never holds a command back.
    Wish,
}

impl Voice {
    /// Whether a memory in this voice may hold a command back (#118): any
    /// memory about the work, never an observation about the user.
    pub(crate) fn holds(self) -> bool {
        self != Self::Wish
    }
}

/// A passage as a hook shows it: whose word its memory is, the label that
/// says so, and the passage's words, ending in " …" when `partial`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Labelled {
    pub(crate) voice: Voice,
    pub(crate) label: String,
    pub(crate) passage: String,
    pub(crate) partial: bool,
}

/// The label of memory `id` and `matched`'s passage of it, from the store;
/// none when the memory is gone or no longer in force (an end or a passed
/// date since the delivery index was built), is not one a hook delivers,
/// or the passage no longer falls on its text.
pub(crate) fn labelled(
    conn: &Connection,
    id: &str,
    matched: &Matched,
) -> Result<Option<Labelled>, Error> {
    let row = conn
        .query_row(
            concat!(
                "SELECT content,context,source,status,memory_type,\
                 (SELECT COUNT(*) FROM memory_origins WHERE memory_id=memories.id) \
                 FROM memories WHERE id=?1 AND ",
                live!("memories")
            ),
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5).and_then(|count| {
                        usize::try_from(count)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(5, count))
                    })?,
                ))
            },
        )
        .optional()?;
    let Some((content, context, source, status, memory_type, origins)) = row else {
        return Ok(None);
    };
    let Some((voice, label)) = label(&source, &status, &memory_type, origins) else {
        return Ok(None);
    };
    let passage = matcher::cut(&content, &context, matched);
    if passage.is_empty() {
        return Ok(None);
    }
    let whole = if matched.slot == super::index::passages::CONTEXT_SLOT {
        &context
    } else {
        &content
    };
    let partial = passage.len() < whole.trim().len();
    let more = if partial { MORE } else { "" };
    let passage = passage.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(Some(Labelled {
        voice,
        label,
        passage: format!("{passage}{more}"),
        partial,
    }))
}

/// Whose word a memory is, in the voice of every text for agents: the
/// user's rule or decision, any other stated memory by its type, or an
/// agent's note with how many conversations learned it. An agent's note
/// that [`reads_as_wish`] says it is not the user's rule, as its `note` in
/// a read does; one about the work (a fact, decision, goal or lesson)
/// names its type. None for an inference, which is never pushed.
pub(super) fn label(
    source: &str,
    status: &str,
    memory_type: &str,
    origins: usize,
) -> Option<(Voice, String)> {
    match source {
        "explicit" if is_rule(source, status, memory_type) => {
            Some((Voice::Rule, "The user's rule".to_owned()))
        }
        "explicit" => Some((Voice::User, format!("The user's {memory_type}"))),
        "observed" => {
            let sessions = origins.max(1);
            if reads_as_wish(memory_type) {
                let unit = if sessions == 1 { "session" } else { "sessions" };
                Some((
                    Voice::Wish,
                    format!("An agent's note, learned in {sessions} {unit}, not the user's rule"),
                ))
            } else if sessions == 1 {
                Some((
                    Voice::Note,
                    format!("An agent's note from an earlier session ({memory_type})"),
                ))
            } else {
                Some((
                    Voice::Note,
                    format!("An agent's note from {sessions} earlier sessions ({memory_type})"),
                ))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        delivery::{DeliveryIndex, Source, SourceRow},
        index::stems::{Field, Stems},
        matcher::{Action, Kind, Window},
    };
    use std::time::Duration;

    /// A server's store, or a store not read before the hook's deadline,
    /// gives each passing match a line naming its id alone: no label, no
    /// text.
    #[test]
    fn without_the_store_a_push_names_the_id_alone() {
        const A: &str = "e0000001-0000-4000-8000-000000000001";
        let row = |slot, words: &str, anchors: &[&str]| SourceRow {
            slot,
            start: 0,
            length: 40,
            stems: Stems::new()
                .all(words)
                .into_iter()
                .map(|s| (s, Field::Body))
                .collect(),
            anchors: anchors.iter().map(|a| (*a).to_owned()).collect(),
        };
        let note = Source {
            id: A.to_owned(),
            rows: vec![row(0, "", &[]), row(1, "cargo publish", &["cargo publish"])],
        };
        let index = DeliveryIndex::read(DeliveryIndex::write(&[note]).unwrap()).unwrap();
        let view = View::new(index, None).unwrap();
        let mut window = Window::default();
        window.push(&Action {
            kind: Kind::Command,
            text: "cargo publish".to_owned(),
            failed: false,
        });
        let found = view.rank(&window).unwrap();
        let passing: Vec<&Matched> = found.iter().filter(|m| passes(m, false)).collect();
        assert_eq!(passing.len(), 1);
        let later = Instant::now() + Duration::from_secs(60);
        let lines = lines(None, &view, &passing, later).unwrap();
        let pointer = format!("- e0000001 {POINTER}");
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            [pointer.as_str()]
        );
        assert_eq!((lines[0].id.as_str(), lines[0].passage), (A, 1));
        // A store, but the deadline passed: the same line, the store unread.
        let store = Connection::open_in_memory().unwrap();
        let late = super::lines(Some(&store), &view, &passing, Instant::now()).unwrap();
        assert_eq!(late, lines);
    }

    #[test]
    fn each_source_is_labelled_by_whose_word_it_is() {
        for (source, status, memory_type, origins, want) in [
            ("explicit", "confirmed", "preference", 0, "The user's rule"),
            ("explicit", "confirmed", "constraint", 1, "The user's rule"),
            (
                "explicit",
                "confirmed",
                "decision",
                1,
                "The user's decision",
            ),
            ("explicit", "confirmed", "goal", 1, "The user's goal"),
            ("explicit", "confirmed", "fact", 1, "The user's fact"),
            (
                "observed",
                "tentative",
                "lesson",
                0,
                "An agent's note from an earlier session (lesson)",
            ),
            (
                "observed",
                "tentative",
                "fact",
                2,
                "An agent's note from 2 earlier sessions (fact)",
            ),
            (
                "observed",
                "tentative",
                "preference",
                2,
                "An agent's note, learned in 2 sessions, not the user's rule",
            ),
            (
                "observed",
                "tentative",
                "constraint",
                1,
                "An agent's note, learned in 1 session, not the user's rule",
            ),
        ] {
            assert_eq!(
                label(source, status, memory_type, origins)
                    .map(|(_, label)| label)
                    .as_deref(),
                Some(want),
                "{source} {memory_type}"
            );
        }
        assert_eq!(label("inferred", "tentative", "fact", 2), None);
        // Whose word it is decides whether it may hold a command back: an
        // observation about the user never does (#118).
        let voice =
            |source, memory_type| label(source, "confirmed", memory_type, 1).map(|(v, _)| v);
        assert_eq!(voice("explicit", "preference"), Some(Voice::Rule));
        assert_eq!(voice("explicit", "fact"), Some(Voice::User));
        assert_eq!(voice("observed", "fact"), Some(Voice::Note));
        assert_eq!(voice("observed", "preference"), Some(Voice::Wish));
        assert!(!Voice::Wish.holds());
        assert!(
            [Voice::Rule, Voice::User, Voice::Note]
                .iter()
                .all(|v| v.holds())
        );
    }

    #[test]
    fn a_push_opens_with_its_header_and_a_line_per_memory() {
        let line = |id: &str, text: &str| Line {
            id: id.to_owned(),
            passage: 1,
            text: text.to_owned(),
        };
        let lines = [
            line(
                "e0000005",
                "- e0000005 An agent's note from 2 earlier sessions (lesson): Run the tests before every commit",
            ),
            line(
                "e0000001",
                "- e0000001 The user's rule: Ask before every git push",
            ),
        ];
        assert_eq!(
            text(&lines, false),
            format!("{HEADER}\n{}\n{}", lines[0].text, lines[1].text)
        );
        assert!(text(&lines, true).starts_with("From user-memory, matching this failed step"));
        assert_eq!(header_chars(false), HEADER.chars().count());
    }
}
