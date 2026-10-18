//! Titles for memories stored before titles existed. The migration to schema
//! version 3 and imports of format version 1 files take them as a map from
//! memory id to title, written by an agent and approved by the user, and
//! refuse until every memory has a valid one. Nothing derives a title: its
//! wording decides how agents understand the memory.

use super::{TITLE_LIMIT, clean};
use anyhow::{Context, Result, bail};
use serde::{
    Deserializer,
    de::{self, MapAccess, Visitor},
};
use std::{collections::BTreeMap, fmt};

/// A titles map: memory id to title.
pub type Titles = BTreeMap<String, String>;

/// The titles map in `text`, a JSON object mapping each memory id to its title.
pub fn parse_titles(text: &str) -> Result<Titles> {
    let mut parser = serde_json::Deserializer::from_str(text);
    let titles = unique(&mut parser)
        .and_then(|titles| parser.end().map(|()| titles))
        .context("A titles map must be a JSON object mapping each memory id to its title")?;
    Ok(titles)
}

/// A titles map whose ids each appear once: JSON keeps the last of repeated
/// keys, which would silently drop a title the user approved.
fn unique<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Titles, D::Error> {
    struct Unique;
    impl<'de> Visitor<'de> for Unique {
        type Value = Titles;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a JSON object mapping each memory id to its title")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Titles, A::Error> {
            let mut titles = Titles::new();
            let mut repeated = Vec::new();
            while let Some((id, title)) = map.next_entry::<String, String>()? {
                if titles.contains_key(&id) && !repeated.contains(&id) {
                    repeated.push(id.clone());
                }
                titles.insert(id, title);
            }
            if repeated.is_empty() {
                Ok(titles)
            } else {
                Err(de::Error::custom(format!(
                    "the titles map names these ids more than once: {}",
                    repeated.join(", ")
                )))
            }
        }
    }
    deserializer.deserialize_map(Unique)
}

/// [`unique`] for an optional field, where null means no map.
pub(super) fn unique_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Titles>, D::Error> {
    struct Optional;
    impl<'de> Visitor<'de> for Optional {
        type Value = Option<Titles>;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("null or a JSON object mapping each memory id to its title")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            unique(deserializer).map(Some)
        }
    }
    deserializer.deserialize_option(Optional)
}

/// Why `title` cannot be stored as it is, or `None`. A title is stored byte
/// for byte, so one that cleaning would change is refused, not rewritten.
fn problem(title: &str) -> Option<String> {
    let length = title.chars().count();
    if title.is_empty() {
        Some("the title is empty".into())
    } else if length > TITLE_LIMIT {
        Some(format!(
            "the title has {length} characters; the limit is {TITLE_LIMIT}"
        ))
    } else {
        None
    }
}

/// Each memory's cleaned title from `titles`, for `memories` given as (id,
/// content). Refuses, listing every untitled memory with its content (left
/// out when empty), every id the map names that is not among `memories` and
/// every title empty or too long once cleaned, unless the map titles each
/// memory. `task` names what needs the titles and `retry` how to supply them.
pub(super) fn assign(
    memories: &[(String, String)],
    titles: Option<&Titles>,
    task: &str,
    retry: &str,
) -> Result<BTreeMap<String, String>> {
    // Whitespace is tidied, as in any other title: trimmed, with runs and
    // line breaks collapsed to one space; the cleaned title is checked.
    let titles: Titles = titles
        .into_iter()
        .flatten()
        .map(|(id, title)| (id.clone(), clean(title, usize::MAX)))
        .collect();
    let titles = &titles;
    let untitled: Vec<&(String, String)> = memories
        .iter()
        .filter(|(id, _)| !titles.contains_key(id))
        .collect();
    let unknown: Vec<&String> = titles
        .keys()
        .filter(|id| !memories.iter().any(|(known, _)| known == *id))
        .collect();
    let invalid: Vec<(&String, String)> = titles
        .iter()
        .filter_map(|(id, title)| problem(title).map(|why| (id, why)))
        .collect();
    if untitled.is_empty() && unknown.is_empty() && invalid.is_empty() {
        return Ok(titles.clone());
    }
    let mut message = format!(
        "{task} needs a title for every memory: one line of at most {TITLE_LIMIT} characters stating it, proposed by an agent and approved by the user. Nothing was changed.\nGive {retry} a JSON object mapping each memory id to its title."
    );
    if !untitled.is_empty() {
        message += &format!("\nUntitled memories ({}):", untitled.len());
        for (id, content) in untitled {
            message += &format!("\n  {id}");
            if !content.is_empty() {
                message += &format!("  {content}");
            }
        }
    }
    if !unknown.is_empty() {
        message += &format!(
            "\nIds the map names that are not memories here ({}):",
            unknown.len()
        );
        for id in unknown {
            message += &format!("\n  {id}");
        }
    }
    if !invalid.is_empty() {
        message += &format!("\nInvalid titles ({}):", invalid.len());
        for (id, why) in invalid {
            message += &format!("\n  {id}: {why}");
        }
    }
    bail!(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memories() -> Vec<(String, String)> {
        vec![
            ("a".into(), "Prefer Rust for command-line tools".into()),
            ("b".into(), "Answer briefly".into()),
        ]
    }

    fn map(pairs: &[(&str, &str)]) -> Titles {
        pairs
            .iter()
            .map(|(id, title)| ((*id).to_owned(), (*title).to_owned()))
            .collect()
    }

    #[test]
    fn a_full_valid_map_titles_every_memory_as_written() {
        let titles = map(&[("a", "Rust for CLIs"), ("b", &"é".repeat(TITLE_LIMIT))]);
        assert_eq!(
            assign(&memories(), Some(&titles), "Task", "it").unwrap(),
            titles
        );
        assert_eq!(assign(&[], None, "Task", "it").unwrap(), Titles::new());
    }

    #[test]
    fn every_gap_and_bad_title_is_listed() {
        let err = assign(&memories(), None, "Task", "`x --titles FILE`")
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Task needs a title for every memory"),
            "{err}"
        );
        assert!(
            err.contains("Give `x --titles FILE` a JSON object"),
            "{err}"
        );
        assert!(err.contains("Untitled memories (2):\n  a  Prefer Rust for command-line tools\n  b  Answer briefly"), "{err}");
        let titles = map(&[
            ("a", ""),
            ("b", &"x".repeat(TITLE_LIMIT + 1)),
            ("c", "Stray"),
        ]);
        let err = assign(&memories(), Some(&titles), "Task", "it")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("Untitled"), "{err}");
        let ids_only = assign(&[("c".into(), String::new())], None, "Task", "it")
            .unwrap_err()
            .to_string();
        assert!(
            ids_only.ends_with("Untitled memories (1):\n  c"),
            "{ids_only}"
        );
        assert!(err.contains("not memories here (1):\n  c"), "{err}");
        assert!(err.contains("  a: the title is empty"), "{err}");
        assert!(
            err.contains("  b: the title has 121 characters; the limit is 120"),
            "{err}"
        );
        // Untidy titles are cleaned, not refused; blank ones are empty.
        for messy in [" Rust  for\tCLIs ", "Rust\nfor CLIs", "Rust for CLIs"] {
            let titles = map(&[("a", messy), ("b", "Brief")]);
            let assigned = assign(&memories(), Some(&titles), "Task", "it").unwrap();
            assert_eq!(assigned["a"], "Rust for CLIs", "{messy:?}");
        }
        let err = assign(
            &memories(),
            Some(&map(&[("a", " \n "), ("b", "Brief")])),
            "Task",
            "it",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("  a: the title is empty"), "{err}");
        let padded = format!("  {}  ", "x".repeat(TITLE_LIMIT));
        let titles = map(&[("a", padded.as_str()), ("b", "Brief")]);
        let assigned = assign(&memories(), Some(&titles), "Task", "it").unwrap();
        assert_eq!(assigned["a"], "x".repeat(TITLE_LIMIT));
    }

    #[test]
    fn a_titles_map_names_each_id_once() {
        let err = format!(
            "{:#}",
            parse_titles(r#"{"a":"x","b":"y","a":"z","b":"w","c":"v"}"#).unwrap_err()
        );
        assert!(
            err.contains("the titles map names these ids more than once: a, b"),
            "{err}"
        );
        // The import tool's titles argument too.
        let err = serde_json::from_str::<crate::memory::ImportMemories>(
            r#"{"name":"old.json","titles":{"a":"x","a":"y"}}"#,
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("more than once: a"), "{err}");
        for absent in [
            r#"{"name":"old.json"}"#,
            r#"{"name":"old.json","titles":null}"#,
        ] {
            let input: crate::memory::ImportMemories = serde_json::from_str(absent).unwrap();
            assert!(input.titles.is_none(), "{absent}");
        }
    }

    #[test]
    fn a_titles_map_is_a_json_object_of_strings() {
        assert_eq!(
            parse_titles(r#"{"a":"Rust"}"#).unwrap(),
            map(&[("a", "Rust")])
        );
        for bad in ["[]", r#"{"a":1}"#, "not json"] {
            let err = parse_titles(bad).unwrap_err().to_string();
            assert!(err.contains("A titles map must be a JSON object"), "{err}");
        }
        let err = format!("{:#}", parse_titles("[]").unwrap_err());
        assert!(
            err.contains("expected a JSON object mapping each memory id to its title"),
            "{err}"
        );
        let err = serde_json::from_str::<crate::memory::ImportMemories>(
            r#"{"name":"old.json","titles":5}"#,
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(
            err.contains("expected a JSON object mapping each memory id to its title"),
            "{err}"
        );
    }
}
