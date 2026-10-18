//! Which memories a request covers: global memories plus the requested
//! projects' by default, narrowed by optional filters that index, read and
//! search share. Filters apply before pagination, so a cursor pages through
//! the filtered result.

use super::{
    Memory, Source, clean, index, project_key,
    schema::{SELECT, row},
    similar::{aliased_categories, category_group},
};
use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use rusqlite::{Connection, params_from_iter, types::Value as Sql};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Which scopes a request covers beyond the default (global memories plus the
/// requested projects').
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScopeFilter {
    /// Global memories only.
    Global,
    /// Only the requested projects' memories.
    Project,
    /// Every memory of every project, for review or inspection.
    All,
}

/// Which statuses a request covers; active memories unless asked otherwise.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, ValueEnum, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
pub enum StatusFilter {
    /// Confirmed and tentative memories.
    #[default]
    Active,
    /// Patterns whose supporting memories changed or were forgotten.
    Stale,
    /// Memories the user replaced.
    Superseded,
    /// Every status.
    All,
}

impl StatusFilter {
    fn statuses(self) -> &'static [&'static str] {
        match self {
            Self::Active => &["confirmed", "tentative"],
            Self::Stale => &["stale"],
            Self::Superseded => &["superseded"],
            Self::All => &["confirmed", "tentative", "stale", "superseded"],
        }
    }
    pub(super) fn covers(self, status: &str) -> bool {
        self.statuses().contains(&status)
    }
}

/// The filters index, read and search share. Without any, a request covers the
/// active global memories and those of `projects`.
#[derive(Args, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct Selection {
    /// Projects whose memories join the global ones: project keys or git remote URLs, as `user-memory project-id` prints them, never local paths. Another project's memories appear only when listed here.
    // --projects is the tools' own name for it, which agents copy into the CLI.
    // The CLI resolves a folder to its project, which the tools refuse.
    #[arg(
        long = "project",
        visible_alias = "projects",
        help = "Projects whose memories join the global ones: project keys, git remote URLs, or a folder such as . whose project is used. Another project's memories appear only when listed here."
    )]
    #[serde(default)]
    pub projects: Vec<String>,
    /// Narrow or widen the scopes: global, project (only the listed projects'), or all (every project, for review or inspection).
    #[arg(long, value_enum)]
    #[serde(default)]
    pub scope: Option<ScopeFilter>,
    /// Only this category; synonyms such as coding/programming match each other.
    #[arg(long)]
    #[serde(default)]
    pub category: Option<String>,
    /// Only this type, such as preference, constraint, goal or pattern.
    #[arg(long = "type")]
    #[serde(rename = "type", default)]
    pub memory_type: Option<String>,
    /// Only this provenance: explicit, observed or inferred.
    #[arg(long, value_enum)]
    #[serde(default)]
    pub source: Option<Source>,
    /// active (the default), stale, superseded or all.
    #[arg(long, value_enum, default_value = "active")]
    #[serde(default)]
    pub status: StatusFilter,
    /// Only memories stated in this conversation, such as codex:thread/<id>.
    #[arg(long)]
    #[serde(default)]
    pub origin: Option<String>,
}

/// Whether `raw` names a place on this machine rather than a project: a
/// remote server could not resolve it, and it would differ between devices.
/// Windows forms (`C:\`, `C:/`, `\\server\`, `.\`) count on every
/// platform, since no project key starts like one.
pub fn local_path(raw: &str) -> bool {
    let raw = raw.trim();
    let drive = matches!(
        raw.as_bytes(),
        [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic()
    );
    raw == "."
        || raw == ".."
        || drive
        || ["/", "./", "../", "~", "file:", "\\\\", ".\\", "..\\"]
            .iter()
            .any(|prefix| raw.starts_with(prefix))
}

/// Refuses `projects` that name places on this machine rather than projects.
pub(super) fn refuse_local_paths(projects: &[String]) -> Result<()> {
    if let Some(path) = projects.iter().find(|p| local_path(p)) {
        bail!(
            "{path} is a local path; give projects as project keys or git remote URLs (`user-memory project-id` prints the key)"
        );
    }
    Ok(())
}

/// Which memories an export or import covers: read's filters, each
/// optional. Without any it covers every memory; with some, an unset status
/// means every status and an unset scope with no project every scope.
#[derive(Args, Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct Subset {
    /// Only these projects' memories, with the global ones unless scope says otherwise: project keys or git remote URLs, never local paths.
    #[arg(
        long = "project",
        help = "Only these projects' memories, with the global ones unless scope says otherwise: project keys, git remote URLs, or a folder such as . whose project is used"
    )]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    /// global, project (only the listed projects') or all.
    #[arg(long, value_enum)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeFilter>,
    /// Only this category; synonyms such as coding/programming match each other.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Only this type, such as preference, constraint, goal or pattern.
    #[arg(long = "type")]
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    /// Only this provenance: explicit, observed or inferred.
    #[arg(long, value_enum)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// active, stale, superseded or all (the default here).
    #[arg(long, value_enum)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusFilter>,
    /// Only memories stated in this conversation, such as codex:thread/<id>.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Subset {
    /// Whether no filter is set, so every memory is covered.
    pub fn is_everything(&self) -> bool {
        *self == Self::default()
    }

    /// The store query's filter, `None` for every memory.
    pub(super) fn filter(&self) -> Result<Option<Filter>> {
        if self.is_everything() {
            return Ok(None);
        }
        let scope = self
            .scope
            .or_else(|| self.projects.is_empty().then_some(ScopeFilter::All));
        Selection {
            projects: self.projects.clone(),
            scope,
            category: self.category.clone(),
            memory_type: self.memory_type.clone(),
            source: self.source,
            status: self.status.unwrap_or(StatusFilter::All),
            origin: self.origin.clone(),
        }
        .filter()
        .map(Some)
    }
}

/// A request's selection in the form the store query takes.
pub(super) struct Filter {
    pub(super) projects: Vec<String>,
    pub(super) scope: Option<ScopeFilter>,
    pub(super) memory_type: String,
    pub(super) categories: Vec<String>,
    pub(super) source: Option<Source>,
    pub(super) status: StatusFilter,
    pub(super) origin: String,
}

impl Selection {
    /// The canonical filter, or why the selection cannot be served.
    pub(super) fn filter(&self) -> Result<Filter> {
        refuse_local_paths(&self.projects)?;
        let projects: Vec<String> = self
            .projects
            .iter()
            .map(|p| project_key(p))
            .filter(|p| !p.is_empty())
            .collect();
        if self.scope == Some(ScopeFilter::Project) && projects.is_empty() {
            bail!("scope project needs projects to list");
        }
        Ok(Filter {
            projects,
            scope: self.scope,
            memory_type: self
                .memory_type
                .as_deref()
                .map(clean_label)
                .unwrap_or_default(),
            categories: self
                .category
                .as_deref()
                .filter(|c| !c.trim().is_empty())
                .map(category_group)
                .unwrap_or_default(),
            source: self.source,
            status: self.status,
            origin: self
                .origin
                .as_deref()
                .map(|o| clean(o, usize::MAX))
                .unwrap_or_default(),
        })
    }

    /// Whether this selection sets a filter that `other` leaves unset.
    pub(super) fn narrows_beyond(&self, other: &Selection) -> bool {
        (!self.projects.is_empty() && other.projects.is_empty())
            || (self.scope.is_some() && other.scope.is_none())
            || (self.category.is_some() && other.category.is_none())
            || (self.memory_type.is_some() && other.memory_type.is_none())
            || (self.source.is_some() && other.source.is_none())
            || self.status != other.status
            || (self.origin.is_some() && other.origin.is_none())
    }
}

fn clean_label(label: &str) -> String {
    clean(label, usize::MAX).to_lowercase()
}

/// Memories the filter covers. With `words`, only those whose text contains
/// one of them or whose category one aliases.
pub(super) fn candidates(
    conn: &Connection,
    filter: &Filter,
    words: Option<&BTreeSet<String>>,
) -> Result<Vec<Memory>> {
    let scope = match filter.scope {
        None => "",
        Some(ScopeFilter::Global) => "global",
        Some(ScopeFilter::Project) => "project",
        Some(ScopeFilter::All) => "all",
    };
    let mut sql = format!(
        "{SELECT} WHERE (CASE ?1 WHEN 'all' THEN 1 WHEN 'global' THEN scope='global' WHEN 'project' THEN scope='project' AND project IN (SELECT value FROM json_each(?2)) ELSE scope='global' OR (scope='project' AND project IN (SELECT value FROM json_each(?2))) END) AND (?3='' OR memory_type=?3) AND (?4='[]' OR category IN (SELECT value FROM json_each(?4))) AND (?5='' OR source=?5) AND status IN (SELECT value FROM json_each(?6)) AND (?7='' OR EXISTS (SELECT 1 FROM memory_origins WHERE memory_id=memories.id AND origin=?7))"
    );
    let mut values = vec![
        Sql::Text(scope.into()),
        Sql::Text(serde_json::to_string(&filter.projects)?),
        Sql::Text(filter.memory_type.clone()),
        Sql::Text(serde_json::to_string(&filter.categories)?),
        Sql::Text(filter.source.map_or("", Source::name).into()),
        Sql::Text(serde_json::to_string(filter.status.statuses())?),
        Sql::Text(filter.origin.clone()),
    ];
    if let Some(words) = words {
        sql.push_str(" AND (id IN (SELECT id FROM memory_search WHERE memory_search MATCH ?8) OR category IN (SELECT value FROM json_each(?9)))");
        let tokens: Vec<&str> = words.iter().map(String::as_str).collect();
        values.push(Sql::Text(index::any_token(&tokens)));
        values.push(Sql::Text(serde_json::to_string(&aliased_categories(
            words,
        ))?));
    }
    Ok(conn
        .prepare(&sql)?
        .query_map(params_from_iter(values), row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths_are_told_apart_from_project_keys() {
        for path in [
            "/srv/repo",
            "./repo",
            "../repo",
            "~/repo",
            ".",
            "..",
            "file:///srv/repo.git",
            " /x ",
            r"C:\Users\u\repo",
            "c:/repo",
            r"\\server\share\repo",
            r".\repo",
            r"..\repo",
        ] {
            assert!(local_path(path), "{path}");
        }
        for key in [
            "github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "https://github.com/owner/repo",
            "git-root:abc123",
            "notebooks",
            "c:repo",
            "12:/x",
        ] {
            assert!(!local_path(key), "{key}");
        }
    }
}
