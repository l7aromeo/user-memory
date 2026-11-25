//! What a negation does to keyword search: among memories whose keyword
//! scores are equal, the one whose negations agree with the topic's ranks
//! first. "Run the slow tests in CI, not before every commit" holds
//! `before` and `commit` as a negation, so for "tests to run before a
//! commit" it ranks below an equal match that holds them plainly, and above
//! it for "tests not to run before a commit". A negation changes no score:
//! a rule that answers by forbidding something ("No production deploys on
//! Fridays") still matches as strongly as one that allows it.

use crate::memory::{Memory, index::stems::Stems};
use std::collections::BTreeSet;

/// Words that negate the rest of their clause. They are English, so a
/// memory in another language gets no negation reading and its ties keep
/// recency. `t` is what is left of "n't" once words are split at the
/// apostrophe ("don't" is "don" and "t"); a stray `t`, as in "AT&T", reads
/// as one too, which is accepted because a negation only reorders ties.
const NEGATORS: &[&str] = &["not", "no", "never", "without", "nor", "cannot", "t"];

/// Where a clause ends, and with it a negation's reach: "not in CI, before
/// every commit" negates only `ci`.
const CLAUSE_ENDS: &[char] = &[',', '.', ';', ':', '!', '?', '(', ')'];

/// The stems a text holds plainly, and those it holds only after a negator
/// in the same clause.
#[derive(Default)]
pub(super) struct Stance {
    plain: BTreeSet<String>,
    negated: BTreeSet<String>,
}

impl Stance {
    pub(super) fn of<'t>(texts: impl IntoIterator<Item = &'t str>, stems: &mut Stems) -> Self {
        let mut plain = BTreeSet::new();
        let mut negated = BTreeSet::new();
        for text in texts {
            for clause in text.split(CLAUSE_ENDS) {
                let lower = clause.to_lowercase();
                let words: Vec<&str> = lower
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .collect();
                let at = words
                    .iter()
                    .position(|w| NEGATORS.contains(w))
                    .unwrap_or(words.len());
                plain.extend(stems.all(&words[..at].join(" ")));
                if let Some(after) = words.get(at + 1..) {
                    negated.extend(stems.all(&after.join(" ")));
                }
            }
        }
        Self { plain, negated }
    }

    /// Whether the text holds `stem`, and if so whether only negated: one
    /// plain use anywhere in it makes it plain.
    fn negates(&self, stem: &str) -> Option<bool> {
        if self.plain.contains(stem) {
            Some(false)
        } else if self.negated.contains(stem) {
            Some(true)
        } else {
            None
        }
    }
}

/// How many of `query`'s stems `memory` holds negated where `topic` does
/// not, or plainly where `topic` negates them. The title decides for a stem
/// it holds, as it outweighs the text; the text, context, reason and
/// evidence decide for the rest.
pub(super) fn disagreements(
    memory: &Memory,
    topic: &Stance,
    query: &[String],
    stems: &mut Stems,
) -> usize {
    let title = Stance::of([memory.title.as_str()], stems);
    let body = Stance::of(
        [
            memory.content.as_str(),
            memory.context.as_str(),
            memory.reason.as_str(),
            memory.evidence.as_str(),
        ],
        stems,
    );
    query
        .iter()
        .filter(|stem| {
            let held = title.negates(stem).or_else(|| body.negates(stem));
            held.is_some_and(|negated| negated != (topic.negates(stem) == Some(true)))
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stance(text: &str) -> Stance {
        Stance::of([text], &mut Stems::new())
    }

    fn stem(word: &str) -> String {
        Stems::new().of(word.to_owned()).to_owned()
    }

    #[test]
    fn a_negator_negates_the_rest_of_its_clause_only() {
        let s = stance("Run the slow tests in CI, not before every commit");
        assert_eq!(s.negates(&stem("tests")), Some(false));
        assert_eq!(s.negates(&stem("before")), Some(true));
        assert_eq!(s.negates(&stem("commit")), Some(true));
        let s = stance("Not in CI, before every commit");
        assert_eq!(s.negates(&stem("ci")), Some(true));
        assert_eq!(s.negates(&stem("commit")), Some(false));
    }

    #[test]
    fn each_negator_and_clause_end_counts() {
        for negator in NEGATORS {
            let text = format!("x {negator} deploy");
            assert_eq!(stance(&text).negates(&stem("deploy")), Some(true), "{text}");
        }
        assert_eq!(stance("don't deploy").negates(&stem("deploy")), Some(true));
        for end in CLAUSE_ENDS {
            let text = format!("never {end} deploy");
            assert_eq!(
                stance(&text).negates(&stem("deploy")),
                Some(false),
                "{text}"
            );
        }
    }

    #[test]
    fn the_title_decides_for_a_stem_it_holds_and_the_text_for_the_rest() {
        let memory = Memory {
            title: "Deploy on Monday".to_owned(),
            content: "Never deploy on Friday".to_owned(),
            ..Memory::default()
        };
        let mut stems = Stems::new();
        let topic = Stance::of(["deploy on friday"], &mut stems);
        let query = [stem("deploy"), stem("friday")];
        // `deploy` is plain in the title, as in the topic; `friday`, which
        // the title lacks, is negated in the text, unlike the topic.
        assert_eq!(disagreements(&memory, &topic, &query, &mut stems), 1);
        let topic = Stance::of(["never deploy on friday"], &mut stems);
        assert_eq!(disagreements(&memory, &topic, &query, &mut stems), 1);
    }

    #[test]
    fn a_stem_held_plainly_anywhere_is_not_negated() {
        let s = stance("Never deploy on Friday. Deploy on Monday");
        assert_eq!(s.negates(&stem("deploy")), Some(false));
        assert_eq!(s.negates(&stem("friday")), Some(true));
        assert_eq!(s.negates(&stem("tuesday")), None);
    }
}
