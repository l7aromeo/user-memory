//! The words of a text as keyword matching sees them: lowercase runs of
//! letters and digits without function words, and the category labels that
//! mean the same.

use std::collections::BTreeSet;

/// Category labels that mean the same at query time. Stored labels are kept
/// as the user gave them; unlisted labels match only themselves.
pub(in crate::memory) const CATEGORY_ALIASES: &[&[&str]] = &[
    &[
        "programming",
        "coding",
        "software development",
        "development",
    ],
    &["communication", "comms", "writing style"],
    &["workflow", "process"],
];

/// Function words carry no topic; one shared "in" must not make two memories
/// look alike or outrank a semantic match.
pub(in crate::memory) const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "could", "do", "does", "for",
    "from", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "of", "on", "or", "our",
    "should", "so", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will",
    "with", "would", "you", "your",
];

/// The lowercase words of `text`, split at anything not a letter or digit,
/// without [`STOP`] words.
pub(in crate::memory) fn tokenize(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}
