//! A memory's passages and their literal anchors.
//!
//! The keyword index holds a memory in passages, so a long memory is found by
//! the one part that matters and search can return that part. A passage is a
//! place in the text (a byte range), never a copy of it. Its anchors are the
//! words that only match as written: code spans, paths, identifiers and a
//! command's name with its subcommand.
//!
//! Also compiled by the delivery benchmark's binary (`user-memory-bench`,
//! evals/delivery/entry.rs) through a `#[path]` mirror of `src/memory`:
//! use only std, rust-stemmers and modules inside that mirror here, or that
//! binary stops building (docs/dev/development.md).

use std::{collections::BTreeSet, ops::Range};

/// Longest passage, in characters, unless one sentence or code span is
/// longer: about a paragraph, which delivery can show whole. A starting
/// value; whole-task measurements may change it.
pub(in crate::memory) const PASSAGE_CHARS: usize = 400;

/// Shortest passage, in characters, unless the text is shorter: a short
/// sentence or list item is joined to the next, as one alone says too
/// little to be returned. A starting value, like [`PASSAGE_CHARS`].
pub(in crate::memory) const PASSAGE_MIN_CHARS: usize = 80;

/// Most passages a text is split into; any beyond go into the last. The
/// longest text recording accepts (4,000 characters) makes at most 50 at
/// [`PASSAGE_MIN_CHARS`]; this bounds what a text written by another
/// program costs the index, and keeps each passage's number in the room the
/// index gives it.
pub(in crate::memory) const MAX_PASSAGES: usize = 250;

/// Rows the keyword index numbers for each memory: the memory's rows are
/// `key * SLOTS` up to the next memory's. Slot 0 holds what counts for the
/// whole memory, slots from 1 the content's passages, and [`CONTEXT_SLOT`]
/// the context's. The keyword index's triggers write this number out.
pub(in crate::memory) const SLOTS: i64 = 256;

/// The slot of a memory's context, a passage of its own: it says when the
/// memory applies.
pub(in crate::memory) const CONTEXT_SLOT: i64 = SLOTS - 1;

const _: () = assert!(
    MAX_PASSAGES as i64 + 1 < CONTEXT_SLOT,
    "every content passage has a slot before the context's"
);

/// Longest file extension a path-shaped word may end in, such as `.toml`
/// or `.swift`; a longer dotted ending is a sentence's, not a file's.
const EXTENSION_CHARS: usize = 5;

/// Characters trimmed from the ends of a word before it is judged as an
/// anchor: quotes, brackets and sentence punctuation around it.
const SURROUNDING: &[char] = &[
    '"', '\'', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':', '!', '?', '.', '`', '*',
];

/// The code spans of `text`: a run of backticks up to the next run of the
/// same length, as Markdown reads them; a run with no match is plain text.
fn code_spans(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let run = |at: usize| bytes[at..].iter().take_while(|&&b| b == b'`').count();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let length = run(at);
        let mut close = at + length;
        let mut matched = None;
        while close < bytes.len() {
            if bytes[close] == b'`' {
                let found = run(close);
                if found == length {
                    matched = Some(close + found);
                    break;
                }
                close += found;
            } else {
                close += 1;
            }
        }
        match matched {
            Some(end) => {
                spans.push(at..end);
                at = end;
            }
            None => at += length,
        }
    }
    spans
}

/// Whether byte offset `at` falls strictly inside one of `spans`.
fn inside(spans: &[Range<usize>], at: usize) -> bool {
    spans.iter().any(|span| span.start < at && at < span.end)
}

/// Whether the word of `text` starting at byte `at` marks a list item: a
/// bullet (`-`, `*`, `+`, `•`) or a number with `.` or `)`, followed by a
/// space. Recording joins a memory's lines with spaces, so a list item
/// shows by its marker.
fn list_marker(text: &str, at: usize) -> bool {
    let word = text[at..].split_whitespace().next().unwrap_or("");
    let followed = text[at + word.len()..].starts_with(char::is_whitespace);
    let numbered = word
        .strip_suffix(['.', ')'])
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    followed && (matches!(word, "-" | "*" | "+" | "•") || numbered)
}

/// The places where a unit of `text` may end: at the start of each line,
/// before a list item's marker, and after the end of a sentence and the
/// space after it, never inside a code span.
fn boundaries(text: &str, spans: &[Range<usize>]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    let mut last_word_end: Option<char> = None;
    let mut newline = false;
    for (at, c) in text.char_indices() {
        if let Some(before) = previous
            && before.is_whitespace()
            && !c.is_whitespace()
            && (newline || matches!(last_word_end, Some('.' | '!' | '?')) || list_marker(text, at))
            && !inside(spans, at)
        {
            found.push(at);
        }
        if c.is_whitespace() {
            newline |= c == '\n';
        } else {
            newline = false;
            last_word_end = Some(c);
        }
        previous = Some(c);
    }
    found
}

/// The places after a space inside `range` of `text`, outside code spans,
/// where an over-long unit may be cut.
fn spaces(text: &str, range: Range<usize>, spans: &[Range<usize>]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    for (offset, c) in text[range.clone()].char_indices() {
        let at = range.start + offset;
        if previous.is_some_and(char::is_whitespace) && !c.is_whitespace() && !inside(spans, at) {
            found.push(at);
        }
        previous = Some(c);
    }
    found
}

fn chars(text: &str, range: &Range<usize>) -> usize {
    text[range.clone()].chars().count()
}

/// Cuts `unit` into pieces of at most [`PASSAGE_CHARS`] at spaces outside
/// code spans; a piece with no such space stays whole.
fn cut(text: &str, unit: Range<usize>, spans: &[Range<usize>]) -> Vec<Range<usize>> {
    if chars(text, &unit) <= PASSAGE_CHARS {
        return vec![unit];
    }
    let mut pieces = Vec::new();
    let mut start = unit.start;
    let mut last_fit = None;
    for at in spaces(text, unit.clone(), spans) {
        if chars(text, &(start..at)) <= PASSAGE_CHARS {
            last_fit = Some(at);
            continue;
        }
        let end = last_fit.unwrap_or(at);
        pieces.push(start..end);
        start = end;
        // A stretch too long to fit, such as a long code span, ends here.
        if at > start && chars(text, &(start..at)) > PASSAGE_CHARS {
            pieces.push(start..at);
            start = at;
        }
        last_fit = (at > start).then_some(at);
    }
    if let Some(end) = last_fit
        && chars(text, &(start..unit.end)) > PASSAGE_CHARS
    {
        pieces.push(start..end);
        start = end;
    }
    pieces.push(start..unit.end);
    pieces
}

/// The passages of `text`, as byte ranges that cover it in order with no
/// gap and no overlap. Units end at the start of a line (a list item, a
/// paragraph) or after a sentence, never inside a code span; units are
/// packed into passages of at most [`PASSAGE_CHARS`], a passage shorter
/// than [`PASSAGE_MIN_CHARS`] taking the next unit too, and a short last
/// one joining the one before, so a passage that can be cut at a space
/// runs at most two short pieces past [`PASSAGE_CHARS`]. A text up to
/// [`PASSAGE_CHARS`] long is one passage; an empty text has none.
pub(in crate::memory) fn split(text: &str) -> Vec<Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }
    let spans = code_spans(text);
    let mut units = Vec::new();
    let mut start = 0;
    for end in boundaries(text, &spans).into_iter().chain([text.len()]) {
        units.extend(cut(text, start..end, &spans));
        start = end;
    }
    let mut passages: Vec<Range<usize>> = Vec::new();
    for unit in units {
        match passages.last_mut() {
            Some(current)
                if chars(text, current) < PASSAGE_MIN_CHARS
                    || chars(text, &(current.start..unit.end)) <= PASSAGE_CHARS =>
            {
                current.end = unit.end;
            }
            _ => passages.push(unit),
        }
    }
    if passages.len() > 1
        && let Some(last) = passages.last()
        && chars(text, last) < PASSAGE_MIN_CHARS
    {
        let last = passages.pop().expect("a last passage");
        passages.last_mut().expect("one before it").end = last.end;
    }
    if passages.len() > MAX_PASSAGES {
        let end = text.len();
        passages.truncate(MAX_PASSAGES);
        passages.last_mut().expect("MAX_PASSAGES is above 0").end = end;
    }
    passages
}

/// Whether `word`, trimmed, is shaped like a path: it holds a `/`, starts
/// with `.` or `~` before more, or is a file name with an extension.
fn path_like(word: &str) -> bool {
    if word.chars().count() < 2 {
        return false;
    }
    if word.contains('/') || word.starts_with('~') || word.starts_with('.') {
        return true;
    }
    match word.rsplit_once('.') {
        Some((name, extension)) => {
            // Two letters around a dot, as in "e.g", abbreviate words.
            name.chars().count() + extension.chars().count() > 2
                && (1..=EXTENSION_CHARS).contains(&extension.chars().count())
                && extension.chars().all(char::is_alphanumeric)
                && extension.chars().any(char::is_alphabetic)
                && name.chars().any(char::is_alphabetic)
        }
        None => false,
    }
}

/// Whether `word` is shaped like an identifier: `snake_case`, `a::b`, or
/// `camelCase` (a lowercase letter followed by an uppercase one).
fn identifier(word: &str) -> bool {
    let snake = word.split('_').filter(|part| !part.is_empty()).count() > 1
        && word.chars().any(char::is_alphabetic);
    let path = word.split("::").filter(|part| !part.is_empty()).count() > 1;
    let camel = word
        .chars()
        .zip(word.chars().skip(1))
        .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
    snake || path || camel
}

/// A word of `text` as an anchor would hold it: without what surrounds it,
/// keeping a leading dot, which starts a path such as `.github`.
fn trimmed(word: &str) -> &str {
    word.trim_start_matches(|c| c != '.' && SURROUNDING.contains(&c))
        .trim_end_matches(SURROUNDING)
}

/// The literal anchors of `text`, lowercased: each code span's word, or a
/// command's name with its next word when that is no flag (`git stash`,
/// `ls` for `ls -la`), and every path- or identifier-shaped word.
pub(in crate::memory) fn anchors(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for span in code_spans(text) {
        let inner = text[span].trim_matches('`').trim();
        let mut words = inner.split_whitespace();
        let Some(first) = words.next() else {
            continue;
        };
        let anchor = match words.next() {
            None => first.to_owned(),
            Some(next) if !next.starts_with('-') => format!("{first} {next}"),
            Some(_) => first.to_owned(),
        };
        found.insert(anchor.to_lowercase());
    }
    for word in text.split_whitespace().map(trimmed) {
        if path_like(word) || identifier(word) {
            found.insert(word.to_lowercase());
        }
    }
    found
}

/// The literal terms a topic may match passages' anchors by: its own
/// anchors, each of its words as written and each pair of its words in a
/// row, lowercased, so `git stash` asked for plainly still finds the
/// command.
pub(in crate::memory) fn query_anchors(topic: &str) -> BTreeSet<String> {
    let mut found = anchors(topic);
    let words: Vec<String> = topic
        .split_whitespace()
        .map(trimmed)
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    for (i, word) in words.iter().enumerate() {
        found.insert(word.clone());
        if let Some(next) = words.get(i + 1) {
            found.insert(format!("{word} {next}"));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seeded xorshift generator.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A text of random sentences, list items, paragraphs and code spans,
    /// some of them long, some with spaces and sentence ends inside.
    fn text(rng: &mut Rng) -> String {
        let words = [
            "alpha",
            "beta",
            "gamma",
            "delta",
            "été",
            "ünï",
            "x",
            "longerword",
        ];
        let mut text = String::new();
        for _ in 0..rng.below(60) {
            match rng.below(10) {
                0 => text.push_str("\n- "),
                1 => text.push_str("\n\n"),
                2 => text.push_str(". "),
                3 => text.push_str("! "),
                4 => {
                    let ticks = "`".repeat(1 + rng.below(3));
                    text.push_str(&ticks);
                    for _ in 0..rng.below(40) {
                        text.push_str(words[rng.below(words.len())]);
                        text.push_str(if rng.below(5) == 0 { ". " } else { " " });
                    }
                    text.push_str(&ticks);
                    text.push(' ');
                }
                5 => text.push('`'),
                _ => {
                    for _ in 0..1 + rng.below(30) {
                        text.push_str(words[rng.below(words.len())]);
                        text.push(' ');
                    }
                }
            }
        }
        text
    }

    // Passages cover every text in order with no gap and no overlap, never
    // cut a code span, keep to their bounds where the text allows, and a
    // text no longer than a passage is one.
    #[test]
    fn passages_cover_the_text_and_never_split_a_code_span() {
        let mut rng = Rng(0x0b5e_55ed);
        for case in 0..2_000 {
            let text = text(&mut rng);
            let passages = split(&text);
            let mut at = 0;
            for passage in &passages {
                assert_eq!(passage.start, at, "case {case}: gap or overlap in {text:?}");
                assert!(passage.end > passage.start, "case {case}: empty passage");
                at = passage.end;
            }
            assert_eq!(at, text.len(), "case {case}: {text:?}");
            let spans = code_spans(&text);
            for passage in &passages[..passages.len().saturating_sub(1)] {
                assert!(
                    !inside(&spans, passage.end),
                    "case {case}: a code span is cut in {text:?}"
                );
            }
            if text.chars().count() <= PASSAGE_CHARS {
                assert!(passages.len() <= 1, "case {case}: {passages:?}");
            }
            if passages.len() > 1 {
                for passage in &passages {
                    assert!(
                        chars(&text, passage) >= PASSAGE_MIN_CHARS,
                        "case {case}: short passage {passage:?} in {text:?}"
                    );
                }
            }
            for passage in &passages {
                // Past its longest stretch that cannot be cut, or
                // PASSAGE_CHARS, a passage holds only short pieces joined
                // on either side.
                let cuts = spaces(&text, passage.clone(), &spans);
                let longest = [passage.start]
                    .into_iter()
                    .chain(cuts.iter().copied())
                    .zip(cuts.iter().copied().chain([passage.end]))
                    .map(|(a, b)| chars(&text, &(a..b)))
                    .max()
                    .unwrap_or(0);
                assert!(
                    chars(&text, passage) <= longest.max(PASSAGE_CHARS) + 2 * PASSAGE_MIN_CHARS,
                    "case {case}: passage {passage:?} of {} characters could be cut in {text:?}",
                    chars(&text, passage)
                );
            }
        }
    }

    // A list of ten points is ten units: each long enough to stand alone
    // becomes its own passage once the list is too long for one.
    #[test]
    fn list_items_and_sentences_end_units() {
        let items: Vec<String> = (1..=10)
            .map(|n| format!("- Point {n}: {}", "detail ".repeat(10)))
            .collect();
        let text = items.join("\n");
        let passages = split(&text);
        assert!(passages.len() > 1);
        for passage in &passages {
            let part = &text[passage.clone()];
            assert!(part.starts_with("- Point"), "{part:?}");
        }
        // Recording joins lines with spaces: markers still end units.
        assert_eq!(split(&items.join(" ")).len(), passages.len());
        assert_eq!(split("One short line."), vec![0..15]);
        assert!(split("").is_empty());
    }

    // More passages than the room the index gives go into the last.
    #[test]
    fn passages_past_the_most_go_into_the_last() {
        // Lines too long for two to share a passage.
        let text = format!("{}\n", "x".repeat(PASSAGE_CHARS * 3 / 4)).repeat(MAX_PASSAGES + 10);
        let passages = split(&text);
        assert_eq!(passages.len(), MAX_PASSAGES);
        assert_eq!(passages.last().unwrap().end, text.len());
    }

    #[test]
    fn anchors_are_code_paths_identifiers_and_commands() {
        let found = anchors(
            "Run `git stash pop` then `ls -la` and `cargo`. Edit src/main.rs, ~/.zshrc and \
             Cargo.toml (see .github/workflows). Call parse_entry or a::b or fetchUser. \
             This is plain text, e.g. nothing else.",
        );
        for anchor in [
            "git stash",
            "ls",
            "cargo",
            "src/main.rs",
            "~/.zshrc",
            "cargo.toml",
            ".github/workflows",
            "parse_entry",
            "a::b",
            "fetchuser",
        ] {
            assert!(found.contains(anchor), "{anchor} in {found:?}");
        }
        for plain in ["git", "plain", "text", "this", "nothing"] {
            assert!(!found.contains(plain), "{plain} in {found:?}");
        }
        assert!(
            !anchors("Version 3.14 and v0.10.0.")
                .iter()
                .any(|a| a.contains('3'))
        );
        assert!(anchors("Plain words, e.g. these, i.e. not files.").is_empty());
    }

    #[test]
    fn a_topic_offers_its_words_and_pairs_as_anchors() {
        let found = query_anchors("How to git stash safely?");
        assert!(found.contains("git stash"), "{found:?}");
        assert!(found.contains("stash"), "{found:?}");
        assert!(found.contains("safely"), "{found:?}");
    }
}
