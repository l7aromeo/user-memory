//! What the pre-tool hook says when it holds a shell command back (#118):
//! the passages whose anchors the command names, shown before it runs as
//! the reason of a deny, labelled by whose word they are, with the way on:
//! run the same command again, which is not held again.
//!
//! A deny is the only text a client puts in front of the agent before a
//! command runs (a pre-tool hook's added context arrives after the tool's
//! result), so the reason says it is deliberate: Claude Code shows it as
//! "PreToolUse:Bash hook error: <reason>", and Codex as "Command blocked by
//! PreToolUse hook: <reason>. Command: <command>".

use super::{
    delivery::{Error, View},
    ids::SHORT_ID,
    matcher::Matched,
    push::{Labelled, Voice, labelled},
};
use rusqlite::Connection;
use std::time::Instant;

/// The most passages one hold shows, strongest first; the others it
/// matched are pushed after the command runs. Measured by #125 at 1,000
/// memories: showing 5 costs 0.01 of anchored recall against no cap, 3
/// would cost 0.08, and dropping the rest rather than pushing them 0.10.
pub(crate) const HOLD_PER_EVENT: usize = 5;

/// The most characters a hold's reason holds: [`HOLD_PER_EVENT`] passages
/// of 400 characters, the most a push shows of one, with their ids and
/// marks.
pub(crate) const HOLD_CHARS: usize = 2_000;

/// The most holding matches a hook reads the text of: twice
/// [`HOLD_PER_EVENT`], so where some may not hold (an observation about
/// the user) the next can take their place, while a command that many
/// passages name never makes the hook read them all.
pub(crate) const HOLD_READ: usize = 2 * HOLD_PER_EVENT;

/// Opens a hold that shows several passages.
const SEVERAL: &str = "Held back once by user-memory to show what matches this command before it runs (not an error, and not the user's refusal):";

/// Ends a hold that shows several passages, of which an agent's note.
const WEIGH: &str = "An agent's note is not the user's rule: weigh it. ";

/// Ends every hold that shows several passages.
const AGAIN: &str =
    "If the command keeps to them, run the same command again; it will not be held again.";

/// One passage a hold may show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    /// The memory's id.
    pub(crate) id: String,
    /// The passage's slot, the key it is held once by.
    pub(crate) passage: i64,
    pub(crate) shown: Labelled,
}

impl Held {
    /// The characters its line takes in a hold of several passages, its
    /// line break included: what it costs in [`HOLD_CHARS`].
    pub(crate) fn chars(&self) -> usize {
        self.line().chars().count() + 1
    }

    fn short(&self) -> &str {
        self.id.get(..SHORT_ID).unwrap_or(&self.id)
    }

    fn line(&self) -> String {
        format!(
            "- {} {}{}",
            self.short(),
            super::push::mark(self.shown.voice),
            self.shown.passage
        )
    }
}

/// The passages of `holding`, matches of `view` strongest first, that may
/// hold a command back, read from the store `conn` until `deadline`: a
/// memory gone, an inference, an observation about the user or a passage
/// no longer on its text has none.
pub(crate) fn lines(
    conn: &Connection,
    view: &View,
    holding: &[&Matched],
    deadline: Instant,
) -> Result<Vec<Held>, Error> {
    let mut held = Vec::with_capacity(holding.len());
    for matched in holding {
        if Instant::now() >= deadline {
            break;
        }
        let id = view.id(matched.memory)?.to_owned();
        if let Some(shown) = labelled(conn, &id, matched)?
            && shown.voice.holds()
        {
            held.push(Held {
                id,
                passage: matched.slot,
                shown,
            });
        }
    }
    Ok(held)
}

/// The characters a hold of several passages takes beside their lines:
/// its opening and closing sentences.
pub(crate) fn frame_chars() -> usize {
    SEVERAL.chars().count() + 1 + WEIGH.chars().count() + AGAIN.chars().count()
}

/// A hold's reason for `held`, at least one passage: one passage in a
/// sentence of its own, several as lines under one opening.
pub(crate) fn reason(held: &[&Held]) -> String {
    if let [one] = held {
        return single(one);
    }
    let mut text = SEVERAL.to_owned();
    for one in held {
        text.push('\n');
        text.push_str(&one.line());
    }
    text.push('\n');
    if held.iter().any(|one| one.shown.voice == Voice::Note) {
        text.push_str(WEIGH);
    }
    text.push_str(AGAIN);
    text
}

/// A hold of one passage, worded as #118 decided.
fn single(held: &Held) -> String {
    let rest = if held.shown.partial {
        ", read it for the rest."
    } else {
        ""
    };
    let passage = &held.shown.passage;
    let short = held.short();
    match held.shown.voice {
        Voice::Note => format!(
            "Held back once by user-memory to show an agent's note before this runs (not an error, not the user's refusal, and not the user's rule: weigh it): {passage} ({short}{rest}) If it does not apply here, run the same command again; it will not be held again."
        ),
        _ => format!(
            "Held back once by user-memory to show {whose} before this runs (not an error, and not the user's refusal): {passage} ({short}{rest}) If the command keeps to it, run the same command again; it will not be held again.",
            whose = lowercase_first(&held.shown.label),
        ),
    }
}

/// `label` with its first letter lowercase, as it reads inside a sentence:
/// "The user's rule" becomes "the user's rule".
fn lowercase_first(label: &str) -> String {
    let mut chars = label.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(voice: Voice, label: &str, passage: &str, partial: bool) -> Held {
        Held {
            id: "e0000001-0000-4000-8000-000000000001".to_owned(),
            passage: 1,
            shown: Labelled {
                voice,
                label: label.to_owned(),
                passage: passage.to_owned(),
                partial,
            },
        }
    }

    #[test]
    fn one_passage_reads_as_the_issue_words_it_by_whose_word_it_is() {
        let note = held(
            Voice::Note,
            "An agent's note from an earlier session (lesson)",
            "Never delete another session's lock file in a chained command. …",
            true,
        );
        assert_eq!(
            reason(&[&note]),
            "Held back once by user-memory to show an agent's note before this runs (not an error, not the user's refusal, and not the user's rule: weigh it): Never delete another session's lock file in a chained command. … (e0000001, read it for the rest.) If it does not apply here, run the same command again; it will not be held again."
        );
        let rule = held(
            Voice::Rule,
            "The user's rule",
            "Push, publish and tag only on the user's go-ahead.",
            false,
        );
        assert_eq!(
            reason(&[&rule]),
            "Held back once by user-memory to show the user's rule before this runs (not an error, and not the user's refusal): Push, publish and tag only on the user's go-ahead. (e0000001) If the command keeps to it, run the same command again; it will not be held again."
        );
        let decision = held(Voice::User, "The user's decision", "Ship from main.", false);
        assert!(
            reason(&[&decision]).starts_with(
                "Held back once by user-memory to show the user's decision before this runs"
            ),
            "{}",
            reason(&[&decision])
        );
    }

    #[test]
    fn several_passages_are_lines_and_an_agents_note_is_to_be_weighed() {
        let rule = held(
            Voice::Rule,
            "The user's rule",
            "Tag only on a go-ahead.",
            false,
        );
        let note = held(
            Voice::Note,
            "An agent's note from an earlier session (fact)",
            "The release script tags as it runs.",
            false,
        );
        let text = reason(&[&rule, &note]);
        assert_eq!(
            text,
            format!(
                "{SEVERAL}\n- e0000001 Tag only on a go-ahead.\n- e0000001 An agent's note: The release script tags as it runs.\n{WEIGH}{AGAIN}"
            )
        );
        let rules = reason(&[&rule, &rule]);
        assert!(!rules.contains(WEIGH), "{rules}");
        assert!(rules.ends_with(AGAIN), "{rules}");
        // What a line costs is what it takes in the reason.
        assert_eq!(
            text.chars().count(),
            frame_chars() + rule.chars() + note.chars()
        );
    }
}
