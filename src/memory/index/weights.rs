//! How much a matched word or anchor counts: by the field that holds it and
//! by how rare it is. Keyword search ranks with these.
//!
//! Also compiled by the delivery benchmark's binary (`user-memory-bench`,
//! evals/delivery/entry.rs) through a `#[path]` mirror of `src/memory`:
//! use only std, rust-stemmers and modules inside that mirror here, or that
//! binary stops building (docs/dev/development.md).

use super::stems::Field;

/// How much a topic word counts, by the strongest field of a memory that
/// holds it; each word counts once per memory. A category names what the
/// memory is about, and a title states the memory in a line, so either
/// beats a word somewhere in the text, and a type such as "preference"
/// beats the text. On the retrieval benchmark (evals/retrieval) the order
/// matters more than the values: these weights score keyword MRR 0.72, and
/// in the simulation the design was chosen with, category 2 or 1 with title
/// 1.5 to 2 came within 0.02 of category 3 with title 2.
const CATEGORY_WEIGHT: f64 = 3.0;
/// See [`CATEGORY_WEIGHT`].
const TITLE_WEIGHT: f64 = 2.0;
/// See [`CATEGORY_WEIGHT`]. Above the text's 1 so the documented order holds,
/// category, then type, then text: the design was measured with type and
/// text both at 1, and on the benchmark 1.5 moves nothing, as topics rarely
/// name a memory's type.
const TYPE_WEIGHT: f64 = 1.5;
/// See [`CATEGORY_WEIGHT`]: content, context, reason and evidence.
const TEXT_WEIGHT: f64 = 1.0;

/// How much a word counts in `field`.
pub(in crate::memory) fn weight(field: Field) -> f64 {
    match field {
        Field::Category => CATEGORY_WEIGHT,
        Field::Title => TITLE_WEIGHT,
        Field::Type => TYPE_WEIGHT,
        Field::Body => TEXT_WEIGHT,
    }
}

/// How much a topic's anchor that a row holds as written (see
/// `passages::anchors`: a code span, a path, an identifier, a command with
/// its subcommand) counts beyond the stems it matches too, times its rarity
/// as a word's weight is. A starting value: whole-task measurements may
/// change it.
pub(in crate::memory) const ANCHOR_WEIGHT: f64 = 1.0;

/// BM25's inverse document frequency of a term `holding` of `all`
/// memories hold.
pub(in crate::memory) fn rarity(all: f64, holding: f64) -> f64 {
    ((all - holding + 0.5) / (holding + 0.5)).ln_1p()
}
