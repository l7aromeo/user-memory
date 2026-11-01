//! The words keyword search matches: each word of a memory stemmed, and kept
//! at the strongest field that holds it, as the keyword index stores them.

use crate::memory::{
    Memory,
    similar::{CATEGORY_ALIASES, STOP, category_group, tokenize},
};
use rust_stemmers::{Algorithm, Stemmer};
use std::collections::{BTreeSet, HashMap};

/// Word stems for one search or one indexing pass, each distinct word
/// stemmed once: a store's memories share most of their vocabulary.
pub(in crate::memory) struct Stems {
    stemmer: Stemmer,
    seen: HashMap<String, String>,
}

impl Stems {
    pub(in crate::memory) fn new() -> Self {
        Self {
            stemmer: Stemmer::create(Algorithm::English),
            seen: HashMap::new(),
        }
    }
    /// The stem of the lowercase `word`, so "release" and "releases" or
    /// "path" and "paths" are one term.
    pub(in crate::memory) fn of(&mut self, word: String) -> &str {
        let stemmer = &self.stemmer;
        self.seen
            .entry(word)
            .or_insert_with_key(|word| stemmer.stem(word).into_owned())
    }
    /// The stems of `text`'s words, without common function words.
    pub(in crate::memory) fn all(&mut self, text: &str) -> BTreeSet<String> {
        tokenize(text)
            .into_iter()
            .map(|word| self.of(word).to_owned())
            .collect()
    }
}

/// Where in a memory a word stands, strongest first: search weighs a word by
/// the strongest field that holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::memory) enum Field {
    /// The category and every label of its alias group.
    Category,
    Title,
    Type,
    /// The content, context, reason and evidence.
    Body,
}

impl Field {
    pub(in crate::memory) const ALL: [Field; 4] =
        [Field::Category, Field::Title, Field::Type, Field::Body];

    /// The keyword index's column holding the words whose strongest field
    /// this is.
    pub(in crate::memory) fn column(self) -> &'static str {
        match self {
            Field::Category => "category",
            Field::Title => "title",
            Field::Type => "type",
            Field::Body => "body",
        }
    }
}

/// The fields of a memory that hold its words.
#[derive(Clone, Copy, Debug)]
pub(in crate::memory) struct Text<'a> {
    pub(in crate::memory) memory_type: &'a str,
    pub(in crate::memory) category: &'a str,
    pub(in crate::memory) title: &'a str,
    pub(in crate::memory) content: &'a str,
    pub(in crate::memory) context: &'a str,
    pub(in crate::memory) reason: &'a str,
    pub(in crate::memory) evidence: &'a str,
}

impl<'a> From<&'a Memory> for Text<'a> {
    fn from(m: &'a Memory) -> Self {
        Self {
            memory_type: &m.memory_type,
            category: &m.category,
            title: &m.title,
            content: &m.content,
            context: &m.context,
            reason: &m.reason,
            evidence: &m.evidence,
        }
    }
}

/// Each stem of `text` with the strongest field holding it.
pub(in crate::memory) fn fields(stems: &mut Stems, text: Text) -> HashMap<String, Field> {
    let labels = category_group(text.category).join(" ");
    let body = format!(
        "{} {} {} {}",
        text.content, text.context, text.reason, text.evidence
    );
    let mut strongest = HashMap::new();
    for (field, words) in
        Field::ALL
            .into_iter()
            .zip([labels.as_str(), text.title, text.memory_type, body.as_str()])
    {
        for word in tokenize(words) {
            strongest.entry(stems.of(word).to_owned()).or_insert(field);
        }
    }
    strongest
}

/// How the keyword index stores `stem`: the lowercase hex of its UTF-8
/// bytes. The index's `ascii` tokenizer keeps a run of hex digits as one
/// token and changes nothing in it, so the index matches exactly the stems
/// this module makes, whatever their script.
pub(in crate::memory) fn token(stem: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(stem.len() * 2);
    for byte in stem.bytes() {
        token.push(char::from(DIGITS[usize::from(byte >> 4)]));
        token.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
    token
}

/// The stemmer crate and version, which `Cargo.lock` must match (a test
/// checks it): another version may stem a word differently.
pub(in crate::memory) const STEMMER: &str = "rust-stemmers 1.2.0";

/// Bumped whenever how words are found changes in a way the rest of
/// [`stamp`] does not show: the tokenizer, the fields or the encoding.
const FORMAT: u32 = 1;

/// Everything the stored stems depend on. The keyword index records the
/// stamp it was built with, and search reads the index only while it
/// matches this binary's; otherwise search reads every memory until the
/// index is rebuilt.
pub(in crate::memory) fn stamp() -> String {
    format!(
        "format {FORMAT}; {STEMMER}; unicode {:?}; stop {STOP:?}; aliases {CATEGORY_ALIASES:?}",
        char::UNICODE_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<'a>(category: &'a str, title: &'a str, content: &'a str) -> Text<'a> {
        Text {
            memory_type: "preference",
            category,
            title,
            content,
            context: "",
            reason: "",
            evidence: "",
        }
    }

    #[test]
    fn a_word_is_kept_at_the_strongest_field_holding_it() {
        let mut stems = Stems::new();
        let found = fields(
            &mut stems,
            text("coding", "Releases need notes", "Write release notes"),
        );
        // An alias of the category counts as the category.
        assert_eq!(found["program"], Field::Category);
        assert_eq!(found["releas"], Field::Title);
        assert_eq!(found["note"], Field::Title);
        assert_eq!(found["write"], Field::Body);
        assert_eq!(found["prefer"], Field::Type);
        assert!(!found.contains_key("the"));
    }

    #[test]
    fn tokens_are_hex_digits_only_and_tell_stems_apart() {
        assert_eq!(token("ab"), "6162");
        assert_eq!(token("é"), "c3a9");
        assert_ne!(token("straß"), token("strass"));
        assert!(
            token("İstanbul")
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn the_stemmer_named_in_the_stamp_is_the_one_built() {
        let lock = include_str!("../../../Cargo.lock");
        let version = lock
            .split("[[package]]")
            .find(|p| p.contains("name = \"rust-stemmers\""))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
            .expect("Cargo.lock lists rust-stemmers");
        assert_eq!(
            format!("rust-stemmers {}", version.trim_matches('"')),
            STEMMER
        );
    }
}
