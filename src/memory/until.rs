//! A memory's condition: the one thing whose happening ends it. A date ends
//! on its own, a tag when a checkout of its project has it, and a text when
//! an agent reports it with evidence (or, for a rule, on the user's yes).
//!
//! This module turns what an agent or the user typed into the three stored
//! columns (`until_kind`, `until_value`, `until_project`) and back into the
//! words a reply shows. The schema's `CHECK` holds the same rules, so a row
//! written any other way is refused too.

use clap::Args;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::project_key;
use super::selection::refuse_local_paths;

/// Longest text condition: it is shown beside the memory's index line, whose
/// title is at most 120 characters, so the two together stay one line's read.
pub const UNTIL_CHARS: usize = 120;
/// Longest tag name: git's own limit on a ref name's component is the file
/// system's (255 bytes on the common ones), and a tag is one ref.
const TAG_LIMIT: usize = 255;
/// The stored form of a date: a UTC instant to the second. Comparing two of
/// these as strings orders them in time, which the `live_memories` view
/// relies on; the schema's `CHECK` enforces the shape.
const STORED_DATE: &str = "%Y-%m-%dT%H:%M:%SZ";
/// A bare day as typed: `YYYY-MM-DD`, ten characters.
const DAY_CHARS: usize = 10;

pub const TWO_KINDS: &str = "until names one kind of condition: a date, a tag or a text";
pub const PROJECT_WITHOUT_TAG: &str =
    "until's project goes with a tag: it names the project whose tags count";
pub const GLOBAL_TAG_NEEDS_PROJECT: &str =
    "A global memory's tag condition names the project whose tags count";
pub const BAD_DATE: &str = "until's date is a day, such as 2026-11-20, which ends at the end of that day in your time zone, or an RFC 3339 time with its offset, such as 2026-11-20T18:00:00+07:00";
pub const DATE_PASSED: &str =
    "until's date has already passed, so this memory would end at once; give a later date";
pub const NO_LOCAL_ZONE: &str = "This machine's time zone could not be found, so a bare day has no end; give until's date as an RFC 3339 time with its offset, such as 2026-11-20T23:59:59+07:00";
pub const HOSTED_DAY: &str = "This server doesn't know your time zone, so a bare day has no end here; send until's date as a full RFC 3339 time with its offset, such as 2026-11-20T23:59:59+07:00";

/// What ends a memory, as typed: exactly one of `date`, `tag` (with its
/// `project` when needed) or `text`. The command line spells the fields
/// `--until-date`, `--until-tag`, `--until-project` and `--until-text`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(description = "Exactly one of date, tag (with project) or text.")]
pub struct Until {
    /// YYYY-MM-DD, through the end of that day in the user's time zone ("for today": today's date); or an RFC 3339 time with its offset.
    #[arg(
        id = "until_date",
        long = "until-date",
        value_name = "DATE",
        help = "End the memory after this day, YYYY-MM-DD, at the end of that day in your time zone; or at an RFC 3339 time with its offset. Give one of --until-date, --until-tag or --until-text."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// A release tag's literal name, never a pattern; in a monorepo, the package's own.
    #[arg(
        id = "until_tag",
        long = "until-tag",
        value_name = "TAG",
        help = "End the memory when a checkout of the project has this tag: the literal name, such as v0.10.0, never a pattern."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// With tag: the project whose tags count, a project key or git remote URL; a global memory must name one.
    #[arg(
        id = "until_project",
        long = "until-project",
        value_name = "PROJECT",
        help = "With --until-tag: the project whose tags count. A project memory defaults to its own; a global memory must name one."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// What has to happen, at most 120 characters.
    #[arg(
        id = "until_text",
        long = "until-text",
        value_name = "TEXT",
        allow_hyphen_values = true,
        help = "End the memory when this happens, at most 120 characters, such as \"the parser's fix ships\"."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A condition as stored: one row's `until_kind`, `until_value` and
/// `until_project`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// A UTC instant in [`STORED_DATE`]'s format.
    Date(String),
    Tag {
        tag: String,
        project: String,
    },
    Text(String),
}

/// Where a bare day is resolved, which decides the zone its end is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// On the user's machine: the system's zone.
    Local,
    /// On a hosted server, whose zone is not the user's: a bare day is
    /// refused, since the local connector resolves it before forwarding.
    Hosted,
}

impl Until {
    /// The stored columns (kind, value, project) of a condition in its
    /// stored form, as [`Until::condition`] leaves it; blank for none.
    pub fn columns(until: Option<&Self>) -> (&'static str, String, String) {
        let Some(until) = until else {
            return ("", String::new(), String::new());
        };
        let owned = |field: &Option<String>| field.clone().unwrap_or_default();
        if until.date.is_some() {
            ("date", owned(&until.date), String::new())
        } else if until.tag.is_some() {
            ("tag", owned(&until.tag), owned(&until.project))
        } else if until.text.is_some() {
            ("text", owned(&until.text), String::new())
        } else {
            ("", String::new(), String::new())
        }
    }

    /// The condition in a reply's words, with a date shown in the user's
    /// zone where the call runs on their machine.
    pub fn echo(&self, zone: Zone) -> String {
        let (kind, value, project) = Self::columns(Some(self));
        let Some(condition) = Condition::from_columns(kind, &value, &project) else {
            return String::new();
        };
        let local = match zone {
            Zone::Local => local_zone().ok(),
            Zone::Hosted => None,
        };
        condition.describe(local.as_ref())
    }

    /// The stored condition for a memory in `memory_project` (blank for a
    /// global memory), or the refusal to give the agent. With `now`, a date
    /// at or before it is refused, as a new condition would end at once;
    /// without, any date is kept, for an import restoring what a store held.
    pub fn condition(
        &self,
        memory_project: &str,
        now: Option<Timestamp>,
    ) -> Result<Condition, String> {
        self.condition_in(memory_project, now, local_zone)
    }

    fn condition_in(
        &self,
        memory_project: &str,
        now: Option<Timestamp>,
        zone: impl FnOnce() -> Result<TimeZone, String>,
    ) -> Result<Condition, String> {
        let date = self.date.as_deref().map(str::trim);
        let tag = self.tag.as_deref().map(str::trim);
        let text = self
            .text
            .as_deref()
            .map(|text| super::clean(text, usize::MAX));
        let kinds =
            usize::from(date.is_some()) + usize::from(tag.is_some()) + usize::from(text.is_some());
        if kinds != 1 {
            return Err(TWO_KINDS.into());
        }
        if self.project.is_some() && tag.is_none() {
            return Err(PROJECT_WITHOUT_TAG.into());
        }
        if let Some(date) = date {
            let stored = resolve_date(date, zone)?;
            if let Some(now) = now
                && stored.as_str() <= stored_now(now).as_str()
            {
                return Err(DATE_PASSED.into());
            }
            return Ok(Condition::Date(stored));
        }
        if let Some(text) = text {
            if text.is_empty() {
                return Err(TWO_KINDS.into());
            }
            let length = text.chars().count();
            if length > UNTIL_CHARS {
                return Err(format!(
                    "until's text has {length} characters; the limit is {UNTIL_CHARS}; say what has to happen in fewer words"
                ));
            }
            return Ok(Condition::Text(text));
        }
        let tag = tag.unwrap_or_default();
        if let Some(problem) = tag_problem(tag) {
            return Err(problem);
        }
        if let Some(named) = &self.project
            && let Err(local) = refuse_local_paths(std::slice::from_ref(named))
        {
            return Err(local.to_string());
        }
        let project = match self.project.as_deref().map(project_key) {
            Some(project) if !project.is_empty() => project,
            _ if !memory_project.is_empty() => memory_project.to_owned(),
            _ => return Err(GLOBAL_TAG_NEEDS_PROJECT.into()),
        };
        Ok(Condition::Tag {
            tag: tag.to_owned(),
            project,
        })
    }

    /// Replaces a bare day with its end as a stored instant, resolved in
    /// `zone`, so a call forwarded to another machine carries the user's
    /// end and never the other machine's. Anything else is left for
    /// [`Until::condition`] to judge.
    pub fn resolve_day(&mut self, zone: Zone) -> Result<(), String> {
        let Some(date) = self.date.as_deref().map(str::trim) else {
            return Ok(());
        };
        if !is_day(date) {
            return Ok(());
        }
        let stored = match zone {
            Zone::Local => resolve_date(date, local_zone)?,
            Zone::Hosted => return Err(HOSTED_DAY.into()),
        };
        self.date = Some(stored);
        Ok(())
    }
}

/// The system's time zone, with its rules (offsets and daylight saving
/// changes) at every date. std has no time zones at all: it knows UTC and
/// durations, so the end of a local day, whose offset depends on the zone's
/// rules at that date, needs a zone database. jiff reads the system's own
/// (`TZ`, `/etc/localtime` and the zoneinfo files on Unix; the Windows zone
/// mapped through its bundled database on Windows).
fn local_zone() -> Result<TimeZone, String> {
    TimeZone::try_system().map_err(|_| NO_LOCAL_ZONE.to_owned())
}

/// Whether `date` has a bare day's shape, `YYYY-MM-DD`.
fn is_day(date: &str) -> bool {
    date.len() == DAY_CHARS
        && date.bytes().enumerate().all(|(at, byte)| match at {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// The stored instant `date` ends at: for a bare day, the first instant of
/// the next day in the zone (so the whole day is in force); otherwise the
/// RFC 3339 instant, to the second.
fn resolve_date(
    date: &str,
    zone: impl FnOnce() -> Result<TimeZone, String>,
) -> Result<String, String> {
    let instant = if is_day(date) {
        let day: Date = date.parse().map_err(|_| BAD_DATE.to_owned())?;
        let next = day.tomorrow().map_err(|_| BAD_DATE.to_owned())?;
        // A day starting in a daylight saving gap starts at the gap's end,
        // which is what `to_zoned_datetime`'s compatible choice gives.
        next.to_zoned_datetime(zone()?)
            .map_err(|_| BAD_DATE.to_owned())?
            .timestamp()
    } else {
        date.parse::<Timestamp>().map_err(|_| BAD_DATE.to_owned())?
    };
    let stored = instant.strftime(STORED_DATE).to_string();
    // Years before 0000 or past 9999 don't fit the stored shape.
    if stored.len() != STORED_DATE_CHARS {
        return Err(BAD_DATE.into());
    }
    Ok(stored)
}

/// Length of a stored instant, `YYYY-MM-DDTHH:MM:SSZ`.
const STORED_DATE_CHARS: usize = 20;

/// `now` in the stored form, to compare with a stored date.
pub fn stored_now(now: Timestamp) -> String {
    now.strftime(STORED_DATE).to_string()
}

/// Why `tag` can't be a tag's literal name, if it can't. The rules are
/// git's for a ref name (`git check-ref-format`), so a name this accepts is
/// one git could have made; a pattern's characters are among those refused.
fn tag_problem(tag: &str) -> Option<String> {
    let refused = tag.is_empty()
        || tag.len() > TAG_LIMIT
        || tag.starts_with(['-', '/', '.'])
        || tag.ends_with(['/', '.'])
        || tag.ends_with(".lock")
        || tag.starts_with("refs/")
        || tag.contains("..")
        || tag.contains("//")
        || tag.contains("/.")
        || tag.contains("@{")
        || tag == "@"
        || tag.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        });
    refused.then(|| {
        format!(
            "until's tag {tag:?} is not a tag name: give the literal name, such as v0.10.0, never a pattern or a refs/ path"
        )
    })
}

impl Condition {
    /// The condition of a stored row, or `None` for a memory without one.
    pub fn from_columns(kind: &str, value: &str, project: &str) -> Option<Self> {
        match kind {
            "date" => Some(Self::Date(value.to_owned())),
            "tag" => Some(Self::Tag {
                tag: value.to_owned(),
                project: project.to_owned(),
            }),
            "text" => Some(Self::Text(value.to_owned())),
            _ => None,
        }
    }

    /// The condition as an agent reads it, the shape `until` takes as
    /// input: `{"date": …}`, `{"tag": …, "project": …}` or `{"text": …}`.
    pub fn to_input(&self) -> Until {
        match self {
            Self::Date(at) => Until {
                date: Some(at.clone()),
                ..Until::default()
            },
            Self::Tag { tag, project } => Until {
                tag: Some(tag.clone()),
                project: Some(project.clone()),
                ..Until::default()
            },
            Self::Text(text) => Until {
                text: Some(text.clone()),
                ..Until::default()
            },
        }
    }

    /// The words a reply shows, such as "until end of 2026-11-11
    /// (2026-11-11T17:00:00Z)". A date is shown in `zone` when there is
    /// one, the user's on their machine, and as stored otherwise.
    pub fn describe(&self, zone: Option<&TimeZone>) -> String {
        match self {
            Self::Date(at) => describe_date(at, zone),
            Self::Tag { tag, project } => format!("until tag {tag} exists in {project}"),
            Self::Text(text) => {
                format!("until an agent reports, with evidence, that {text}")
            }
        }
    }
}

fn describe_date(at: &str, zone: Option<&TimeZone>) -> String {
    let local = zone.and_then(|zone| {
        let zoned = at
            .parse::<Timestamp>()
            .ok()?
            .to_zoned_datetime(zone.clone());
        Some(if zoned.time() == jiff::civil::Time::midnight() {
            let day = zoned.date().yesterday().ok()?;
            format!("until end of {day} ({at})")
        } else {
            format!("until {} ({at})", zoned.strftime("%Y-%m-%d %H:%M"))
        })
    });
    local.unwrap_or_else(|| format!("until {at}"))
}

/// Reads `update`'s `until`, keeping "absent" (keep the condition) apart
/// from `null` (clear it): an absent field never reaches this function and
/// stays `None` by `#[serde(default)]`, and a present one is `Some`.
pub fn present<'de, D: Deserializer<'de>>(input: D) -> Result<Option<Option<Until>>, D::Error> {
    Option::<Until>::deserialize(input).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz;

    fn now() -> Timestamp {
        "2026-11-11T03:00:00Z".parse().expect("a valid instant")
    }

    fn jakarta() -> Result<TimeZone, String> {
        Ok(TimeZone::fixed(tz::offset(7)))
    }

    fn until(date: Option<&str>, tag: Option<&str>, text: Option<&str>) -> Until {
        Until {
            date: date.map(Into::into),
            tag: tag.map(Into::into),
            project: None,
            text: text.map(Into::into),
        }
    }

    #[test]
    fn a_day_ends_at_the_next_local_midnight() {
        let today = until(Some("2026-11-11"), None, None);
        assert_eq!(
            today.condition_in("", Some(now()), jakarta),
            Ok(Condition::Date("2026-11-11T17:00:00Z".into()))
        );
    }

    #[test]
    fn a_day_ends_by_the_zone_rules_at_that_date() {
        let new_york = || TimeZone::get("America/New_York").map_err(|err| err.to_string());
        // Daylight saving time in July (UTC-4), standard time in December
        // (UTC-5): one fixed offset would get one of them wrong.
        assert_eq!(
            resolve_date("2027-07-01", new_york),
            Ok("2027-07-02T04:00:00Z".into())
        );
        assert_eq!(
            resolve_date("2027-12-01", new_york),
            Ok("2027-12-02T05:00:00Z".into())
        );
    }

    #[test]
    fn an_instant_with_an_offset_is_stored_in_utc() {
        assert_eq!(
            resolve_date("2026-11-20T18:00:00+07:00", jakarta),
            Ok("2026-11-20T11:00:00Z".into())
        );
        assert_eq!(
            resolve_date("2026-11-20T18:00:00.75Z", jakarta),
            Ok("2026-11-20T18:00:00Z".into())
        );
    }

    #[test]
    fn a_date_that_is_not_a_day_or_an_instant_is_refused() {
        for date in [
            "2026-11-31",
            "2026-1-1",
            "tomorrow",
            "2026-11-20T18:00:00",
            "20261120",
            "",
        ] {
            assert_eq!(resolve_date(date, jakarta), Err(BAD_DATE.into()), "{date}");
        }
    }

    /// A new condition whose date has passed is refused; an imported one,
    /// checked without a now, is kept as the store held it.
    #[test]
    fn a_date_already_passed_is_refused_unless_imported() {
        for (date, stored) in [
            ("2026-11-10", "2026-11-10T17:00:00Z"),
            ("2026-11-11T03:00:00Z", "2026-11-11T03:00:00Z"),
        ] {
            let given = until(Some(date), None, None);
            assert_eq!(
                given.condition_in("", Some(now()), jakarta),
                Err(DATE_PASSED.into()),
                "{date}"
            );
            assert_eq!(
                given.condition_in("", None, jakarta),
                Ok(Condition::Date(stored.into())),
                "{date}"
            );
        }
    }

    #[test]
    fn exactly_one_kind() {
        for given in [
            until(None, None, None),
            until(Some("2026-12-01"), Some("v1.0.0"), None),
            until(Some("2026-12-01"), None, Some("it ships")),
            until(None, Some("v1.0.0"), Some("it ships")),
            until(None, None, Some("   ")),
        ] {
            assert_eq!(
                given.condition_in("p", Some(now()), jakarta),
                Err(TWO_KINDS.into()),
                "{given:?}"
            );
        }
    }

    #[test]
    fn a_project_goes_only_with_a_tag() {
        let mut given = until(None, None, Some("it ships"));
        given.project = Some("github.com/x/y".into());
        assert_eq!(
            given.condition_in("", Some(now()), jakarta),
            Err(PROJECT_WITHOUT_TAG.into())
        );
    }

    #[test]
    fn a_tag_takes_the_memory_project_or_the_named_one() {
        let mut given = until(None, Some("v0.10.0"), None);
        assert_eq!(
            given.condition_in("github.com/x/app", Some(now()), jakarta),
            Ok(Condition::Tag {
                tag: "v0.10.0".into(),
                project: "github.com/x/app".into()
            })
        );
        assert_eq!(
            given.condition_in("", Some(now()), jakarta),
            Err(GLOBAL_TAG_NEEDS_PROJECT.into())
        );
        given.project = Some("https://GitHub.com/Example/Tool.git".into());
        assert_eq!(
            given.condition_in("", Some(now()), jakarta),
            Ok(Condition::Tag {
                tag: "v0.10.0".into(),
                project: "github.com/example/tool".into()
            })
        );
    }

    #[test]
    fn a_tag_is_a_literal_name() {
        for good in ["v0.10.0", "1.2.3", "parser-v1.4.0", "pkg/v2.0.0"] {
            assert_eq!(tag_problem(good), None, "{good}");
        }
        for bad in [
            "v*",
            "v1.?",
            "v[01]",
            "refs/tags/v1",
            "v 1",
            "-v1",
            "v1.lock",
            "v1/",
            "a..b",
            "x@{1}",
            "@",
        ] {
            assert!(tag_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn a_text_is_cleaned_and_limited() {
        assert_eq!(
            until(None, None, Some("  the fix\n ships ")).condition_in("", Some(now()), jakarta),
            Ok(Condition::Text("the fix ships".into()))
        );
        let long = "x".repeat(UNTIL_CHARS + 1);
        assert_eq!(
            until(None, None, Some(&long)).condition_in("", Some(now()), jakarta),
            Err(format!(
                "until's text has {} characters; the limit is {UNTIL_CHARS}; say what has to happen in fewer words",
                UNTIL_CHARS + 1
            ))
        );
        let exact = "x".repeat(UNTIL_CHARS);
        assert!(
            until(None, None, Some(&exact))
                .condition_in("", Some(now()), jakarta)
                .is_ok()
        );
    }

    #[test]
    fn a_hosted_server_refuses_a_bare_day_but_takes_an_instant() {
        let mut day = until(Some("2026-11-20"), None, None);
        assert_eq!(
            day.resolve_day(Zone::Hosted),
            Err("This server doesn't know your time zone, so a bare day has no end here; send until's date as a full RFC 3339 time with its offset, such as 2026-11-20T23:59:59+07:00".into())
        );
        assert_eq!(day.date.as_deref(), Some("2026-11-20"));
        let mut instant = until(Some("2026-11-20T18:00:00+07:00"), None, None);
        assert_eq!(instant.resolve_day(Zone::Hosted), Ok(()));
        assert_eq!(instant.date.as_deref(), Some("2026-11-20T18:00:00+07:00"));
    }

    #[test]
    fn the_echo_shows_the_local_day() {
        let zone = TimeZone::fixed(tz::offset(7));
        let end = Condition::Date("2026-11-11T17:00:00Z".into());
        assert_eq!(
            end.describe(Some(&zone)),
            "until end of 2026-11-11 (2026-11-11T17:00:00Z)"
        );
        assert_eq!(end.describe(None), "until 2026-11-11T17:00:00Z");
        assert_eq!(
            Condition::Date("2026-11-20T11:00:00Z".into()).describe(Some(&zone)),
            "until 2026-11-20 18:00 (2026-11-20T11:00:00Z)"
        );
    }

    #[test]
    fn null_and_absent_differ() {
        #[derive(Deserialize)]
        struct Input {
            #[serde(default, deserialize_with = "present")]
            until: Option<Option<Until>>,
        }
        let read = |json: &str| {
            serde_json::from_str::<Input>(json)
                .expect("valid input")
                .until
        };
        assert_eq!(read("{}"), None);
        assert_eq!(read(r#"{"until":null}"#), Some(None));
        assert_eq!(
            read(r#"{"until":{"text":"it ships"}}"#),
            Some(Some(until(None, None, Some("it ships"))))
        );
    }
}
