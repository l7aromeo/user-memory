//! Cutoffs for an embedding model that has none built in: the labelled pairs
//! the built-in cutoffs were chosen on ship in the binary, are embedded once
//! with the new model, and give its cutoffs by the rules `CALIBRATED`
//! documents. Results are stored with the store's vectors.

use super::{
    Scope,
    index::embed,
    similar::{self, Draft, Thresholds, cosine, document},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// The labelled short pairs, as `similarity-pairs.json`'s `about` describes.
const SHORT: &str = include_str!("calibration/similarity-pairs.json");
/// The labelled long pairs (content of 1,000–4,000 characters).
const LONG: &str = include_str!("calibration/similarity-long-pairs.json");

/// Smallest distance a cutoff keeps from every pair on either side of it.
pub(super) const MARGIN: f32 = 0.01;

/// A labelled pair of memories.
pub(super) struct Labelled {
    pub(super) class: String,
    pub(super) pair: [Value; 2],
    /// From the long set; the cutoffs are computed from the short pairs only
    /// and checked on both.
    pub(super) long: bool,
}

/// Every labelled pair: the short ones, then the long ones.
pub(super) fn pairs() -> Result<Vec<Labelled>> {
    let mut all = Vec::new();
    for (text, long) in [(SHORT, false), (LONG, true)] {
        let set: Value = serde_json::from_str(text).context("labelled pairs are not JSON")?;
        for p in set["pairs"]
            .as_array()
            .context("labelled pairs lack pairs")?
        {
            all.push(Labelled {
                class: p["class"]
                    .as_str()
                    .context("labelled pair lacks a class")?
                    .to_owned(),
                pair: [p["a"].clone(), p["b"].clone()],
                long,
            });
        }
    }
    Ok(all)
}

/// A labelled memory as the draft it is embedded from.
pub(super) fn draft(v: &Value) -> Draft<'_> {
    let field = |name: &str| v[name].as_str().unwrap_or("");
    Draft {
        memory_type: field("type"),
        category: field("category"),
        content: field("content"),
        context: field("context"),
        reason: field("reason"),
        evidence: field("evidence"),
        scope: Scope::Global,
        project: "",
    }
}

/// Classes that must be listed as possible duplicates.
pub(super) fn flagged(class: &str) -> bool {
    matches!(class, "same" | "contradiction")
}

/// The first pair, by class and cosine, that `cutoffs` misplace by the
/// rules `CALIBRATED` documents: restatements and contradictions at or
/// above `duplicate`, exceptions and related pairs at or above `related`,
/// related and unrelated pairs at or below `duplicate`, each with
/// [`MARGIN`]. Unrelated pairs in one category group are not kept below
/// `related`: no cutoff can, and review only proposes them.
pub(super) fn misplaced(cutoffs: Thresholds, scored: &[(&Labelled, f32)]) -> Option<String> {
    scored.iter().find_map(|(l, score)| {
        let score = *score;
        let fits = match l.class.as_str() {
            "same" | "contradiction" => score >= cutoffs.duplicate + MARGIN,
            "exception" => score >= cutoffs.related + MARGIN,
            "related" => score >= cutoffs.related + MARGIN && score <= cutoffs.duplicate - MARGIN,
            "unrelated" => score <= cutoffs.duplicate - MARGIN,
            _ => false,
        };
        let length = if l.long { "long " } else { "" };
        (!fits).then(|| format!("a {length}{} pair scores {score:.3}", l.class))
    })
}

/// Cutoffs from the short pairs: `duplicate` midway between the highest
/// related or unrelated pair and the lowest restatement or contradiction,
/// `related` [`MARGIN`] below the lowest exception or related pair. `Err`
/// says why they cannot be used: the pairs are not separated well enough,
/// short or long.
pub(super) fn derive(scored: &[(&Labelled, f32)]) -> std::result::Result<Thresholds, String> {
    let short = |keep: fn(&str) -> bool| {
        scored
            .iter()
            .filter(move |(l, _)| !l.long && keep(&l.class))
            .map(|(_, s)| *s)
    };
    let lowest_flagged = short(flagged).fold(f32::MAX, f32::min);
    let highest_apart = short(|c| matches!(c, "related" | "unrelated")).fold(f32::MIN, f32::max);
    let lowest_near = short(|c| matches!(c, "related" | "exception")).fold(f32::MAX, f32::min);
    if [lowest_flagged, lowest_near].contains(&f32::MAX) || highest_apart == f32::MIN {
        return Err("the labelled pairs lack a class".into());
    }
    let cutoffs = Thresholds {
        duplicate: (highest_apart + lowest_flagged) / 2.0,
        related: lowest_near - MARGIN,
    };
    match misplaced(cutoffs, scored) {
        None => Ok(cutoffs),
        Some(pair) => Err(format!(
            "this model does not separate the labelled pairs well enough ({pair} against cutoffs {:.3} and {:.3})",
            cutoffs.duplicate, cutoffs.related
        )),
    }
}

/// Embeds every labelled pair with `config`: the vector length and each
/// pair's cosine, in the order of [`pairs`].
pub(super) fn score(config: &embed::Config) -> Result<(usize, Vec<f32>)> {
    let labelled = pairs()?;
    let texts: Vec<String> = labelled
        .iter()
        .flat_map(|l| l.pair.iter().map(|m| document(&draft(m))))
        .collect();
    let vectors = config.embed(&texts)?;
    let length = vectors.first().map_or(0, Vec::len);
    if vectors.len() != texts.len() || vectors.iter().any(|v| v.len() != length || length == 0) {
        bail!("the embedding endpoint returned vectors of differing or no length");
    }
    Ok((
        length,
        vectors.chunks(2).map(|v| cosine(&v[0], &v[1])).collect(),
    ))
}

/// What duplicate detection uses for one model at one vector length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Calibration {
    /// Cutoffs chosen by hand and built in.
    BuiltIn,
    /// Cutoffs computed from the labelled pairs and stored.
    Calibrated { duplicate: f32, related: f32 },
    /// The model cannot tell the labelled pairs apart: keywords only.
    Unseparable,
    /// Not calibrated yet: keywords only until `embedding set` or `sync`.
    Missing,
}

/// The calibration state for `model` at vector `length`.
pub(super) fn state(conn: &Connection, model: &str, length: usize) -> Result<Calibration> {
    if similar::built_in(model, length).is_some() {
        return Ok(Calibration::BuiltIn);
    }
    let stored: Option<(Option<f64>, Option<f64>)> = conn
        .query_row(
            "SELECT duplicate, related FROM embedding_cutoffs WHERE model=?1 AND length=?2",
            params![model, i64::try_from(length)?],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match stored {
        Some((Some(duplicate), Some(related))) => Calibration::Calibrated {
            duplicate: duplicate as f32,
            related: related as f32,
        },
        Some(_) => Calibration::Unseparable,
        None => Calibration::Missing,
    })
}

/// What [`calibrate`] found.
pub(in crate::memory) struct Calibrated {
    pub(in crate::memory) length: usize,
    pub(in crate::memory) state: Calibration,
    /// Why the pairs could not be used, when they could not.
    pub(in crate::memory) reason: Option<String>,
}

/// Calibrates `config`'s model unless its cutoffs are built in: embeds the
/// labelled pairs, derives the cutoffs and stores them, or stores that the
/// pairs could not be separated.
pub(in crate::memory) fn calibrate(
    conn: &Connection,
    config: &embed::Config,
) -> Result<Calibrated> {
    let (length, scores) = score(config)?;
    let model = config.identity().0.to_owned();
    if similar::built_in(&model, length).is_some() {
        return Ok(Calibrated {
            length,
            state: Calibration::BuiltIn,
            reason: None,
        });
    }
    let labelled = pairs()?;
    let scored: Vec<(&Labelled, f32)> = labelled.iter().zip(scores).collect();
    let (cutoffs, reason) = match derive(&scored) {
        Ok(cutoffs) => (Some(cutoffs), None),
        Err(reason) => (None, Some(reason)),
    };
    let tx = super::schema::write(conn)?;
    tx.execute(
        "INSERT INTO embedding_cutoffs (model,length,duplicate,related) VALUES (?1,?2,?3,?4)
         ON CONFLICT(model,length) DO UPDATE SET duplicate=excluded.duplicate,related=excluded.related,calibrated_at=CURRENT_TIMESTAMP",
        params![
            model,
            i64::try_from(length)?,
            cutoffs.map(|c| f64::from(c.duplicate)),
            cutoffs.map(|c| f64::from(c.related)),
        ],
    )?;
    tx.commit()?;
    Ok(Calibrated {
        length,
        state: match cutoffs {
            Some(c) => Calibration::Calibrated {
                duplicate: c.duplicate,
                related: c.related,
            },
            None => Calibration::Unseparable,
        },
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A score inside each class's range for a model that separates the
    /// pairs: restatements high, exceptions and related pairs in the middle,
    /// unrelated pairs low, each spread so the extremes matter.
    fn separated(l: &Labelled, n: usize) -> f32 {
        let spread = (n % 7) as f32 * 0.01;
        match l.class.as_str() {
            "same" | "contradiction" => 0.86 + spread,
            "exception" => 0.62 + spread,
            "related" => 0.64 + spread,
            _ => 0.40 + spread,
        }
    }

    fn scored(labelled: &[Labelled], f: impl Fn(&Labelled, usize) -> f32) -> Vec<(&Labelled, f32)> {
        labelled
            .iter()
            .enumerate()
            .map(|(n, l)| (l, f(l, n)))
            .collect()
    }

    #[test]
    fn the_labelled_pairs_ship_with_every_class() {
        let labelled = pairs().unwrap();
        for class in ["same", "contradiction", "exception", "related", "unrelated"] {
            assert!(
                labelled.iter().any(|l| !l.long && l.class == class),
                "{class}"
            );
        }
        assert!(labelled.iter().any(|l| l.long));
        assert!(
            labelled
                .iter()
                .all(|l| l.pair.iter().all(|m| !draft(m).content.is_empty()))
        );
    }

    // Cutoffs computed from scores that separate the classes keep every
    // labelled pair on its side with the margin, and sit where the rules put
    // them.
    #[test]
    fn derived_cutoffs_follow_the_documented_rules() {
        let labelled = pairs().unwrap();
        let scored = scored(&labelled, separated);
        let cutoffs = derive(&scored).unwrap();
        assert_eq!(misplaced(cutoffs, &scored), None);
        let short = |classes: &[&str]| {
            scored
                .iter()
                .filter(|(l, _)| !l.long && classes.contains(&l.class.as_str()))
                .map(|(_, s)| *s)
                .collect::<Vec<f32>>()
        };
        let max = |v: Vec<f32>| v.into_iter().fold(f32::MIN, f32::max);
        let min = |v: Vec<f32>| v.into_iter().fold(f32::MAX, f32::min);
        let midpoint =
            (max(short(&["related", "unrelated"])) + min(short(&["same", "contradiction"]))) / 2.0;
        assert!((cutoffs.duplicate - midpoint).abs() < 1e-6);
        assert!((cutoffs.related - (min(short(&["related", "exception"])) - MARGIN)).abs() < 1e-6);
    }

    // A cutoff moved past one restatement misplaces it, and is reported.
    #[test]
    fn a_cutoff_past_a_restatement_is_misplaced() {
        let labelled = pairs().unwrap();
        let scored = scored(&labelled, separated);
        let cutoffs = derive(&scored).unwrap();
        let moved = Thresholds {
            duplicate: 0.86,
            ..cutoffs
        };
        let found = misplaced(moved, &scored).unwrap();
        assert!(
            found.contains("same pair") || found.contains("contradiction pair"),
            "{found}"
        );
    }

    // A model that scores every pair alike cannot be calibrated, and says why.
    #[test]
    fn pairs_a_model_cannot_separate_give_no_cutoffs() {
        let labelled = pairs().unwrap();
        let scored = scored(&labelled, |_, _| 0.5);
        let reason = derive(&scored).unwrap_err();
        assert!(
            reason.starts_with("this model does not separate the labelled pairs well enough"),
            "{reason}"
        );
    }

    // Long pairs are checked too: a long restatement below the cutoff
    // computed from the short pairs makes the model unusable.
    #[test]
    fn a_long_pair_on_the_wrong_side_fails_the_calibration() {
        let labelled = pairs().unwrap();
        let scored = scored(&labelled, |l, n| {
            if l.long && l.class == "same" {
                0.70
            } else {
                separated(l, n)
            }
        });
        let reason = derive(&scored).unwrap_err();
        assert!(reason.contains("a long same pair scores 0.700"), "{reason}");
    }
}
