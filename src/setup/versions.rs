//! Release numbers, and the Claude Code release user-memory's Windows hooks
//! need: setup and doctor read `claude --version` there and say when it is
//! older, or when its output cannot be read.

use std::fmt;

/// A release number `X.Y.Z`, compared part by part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// The version `text` spells, exactly `X.Y.Z`; anything else, such as a
    /// pre-release or a missing part, is `None`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.').map(|part| part.parse::<u64>().ok());
        let version = Self {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
        };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The oldest Claude Code that runs exec-form hooks (a `command` and an
/// `args` array, with no shell), which setup writes on Windows: there the
/// shell form runs under Git Bash, which strips the backslashes from the
/// hook's path.
pub(crate) const MIN_WINDOWS_CLAUDE: Version = Version {
    major: 2,
    minor: 1,
    patch: 139,
};

/// What follows the version on `claude --version`'s line, which tells it
/// from a warning line or another program's output.
const CLAUDE_MARKER: &str = "Claude Code";

/// How many characters of unreadable output the note quotes: enough to
/// recognise it, short enough for one line.
const QUOTED_CHARS: usize = 80;

/// The version in `claude --version`'s output (`2.1.139 (Claude Code)`):
/// the first line whose first word is `X.Y.Z`, with or without a leading
/// `v`, and which names Claude Code. `None` when no line does.
pub(crate) fn claude(output: &str) -> Option<Version> {
    output.lines().map(clean).find_map(|line| {
        let (first, rest) = line.split_once(char::is_whitespace)?;
        let first = first.strip_prefix('v').unwrap_or(first);
        rest.contains(CLAUDE_MARKER)
            .then(|| Version::parse(first))
            .flatten()
    })
}

/// What setup and doctor say about Claude Code's version.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClaudeStep {
    /// Claude Code is too old: a step printed under "Action needed:".
    Action(String),
    /// The version cannot be read, so it may be too old: a line printed as
    /// is, never taken as new enough.
    Note(String),
}

/// What to say about the Claude Code whose `claude --version` printed
/// `output` (empty when it could not run); `None` when it is new enough,
/// and always off Windows, where the hooks need no particular release.
pub(crate) fn claude_step(output: &str, windows: bool) -> Option<ClaudeStep> {
    if !windows {
        return None;
    }
    match claude(output) {
        Some(version) if version >= MIN_WINDOWS_CLAUDE => None,
        Some(version) => Some(ClaudeStep::Action(format!(
            "Claude Code {version} is older than {MIN_WINDOWS_CLAUDE}, which runs user-memory's Windows hooks; run `claude update`"
        ))),
        None => {
            let printed = output
                .lines()
                .map(clean)
                .find(|line| !line.is_empty())
                .map(|line| format!(" (it printed: {})", quote(line)))
                .unwrap_or_default();
            Some(ClaudeStep::Note(format!(
                "Cannot read Claude Code's version from `claude --version`{printed}; user-memory's Windows hooks need Claude Code {MIN_WINDOWS_CLAUDE} or later: run `claude update` if it is older"
            )))
        }
    }
}

/// One line of output without a byte order mark or surrounding spaces.
fn clean(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim()
}

/// `line` as the note quotes it: without control characters, which could
/// move the cursor or change colours, and cut at [`QUOTED_CHARS`].
fn quote(line: &str) -> String {
    let mut chars = line.chars().filter(|c| !c.is_control());
    let mut quoted: String = chars.by_ref().take(QUOTED_CHARS).collect();
    if chars.next().is_some() {
        quoted.push('…');
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(version("2.1.99") < version("2.1.139"));
        assert!(version("2.9.200") < version("2.10.0"));
        assert!(version("2.1.139") < version("3.0.0"));
        assert_eq!(version("2.1.139"), MIN_WINDOWS_CLAUDE);
        assert_eq!(MIN_WINDOWS_CLAUDE.to_string(), "2.1.139");
    }

    #[test]
    fn only_three_numbers_make_a_version() {
        for text in [
            "2.1",
            "2.1.139.1",
            "2.1.139-beta.1",
            "2.1.x",
            "",
            "v2.1.139",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn claude_code_versions_are_read_from_its_line() {
        for output in [
            "2.1.139 (Claude Code)\n",
            "2.1.139 (Claude Code)\r\n",
            "\u{feff}2.1.139 (Claude Code)",
            "v2.1.139 (Claude Code)",
            "  2.1.139 (Claude Code)  ",
            "Warning: a newer release is available\n2.1.139 (Claude Code)\n",
        ] {
            assert_eq!(claude(output), Some(MIN_WINDOWS_CLAUDE), "{output:?}");
        }
    }

    #[test]
    fn output_without_a_claude_code_version_is_unreadable() {
        for output in [
            "",
            "\n\n",
            "2.1.139",
            "2.1.139 (Other Tool)",
            "Claude Code 2.1.139",
            "2.1 (Claude Code)",
            "2.1.139-beta.1 (Claude Code)",
            "'claude' is not recognized as an internal or external command",
        ] {
            assert_eq!(claude(output), None, "{output:?}");
        }
    }

    #[test]
    fn an_older_claude_code_on_windows_needs_an_update() {
        for (output, shown) in [
            ("2.1.138 (Claude Code)", "2.1.138"),
            ("2.1.99 (Claude Code)", "2.1.99"),
            ("1.0.200 (Claude Code)", "1.0.200"),
        ] {
            assert_eq!(
                claude_step(output, true),
                Some(ClaudeStep::Action(format!(
                    "Claude Code {shown} is older than 2.1.139, which runs user-memory's Windows hooks; run `claude update`"
                )))
            );
        }
    }

    #[test]
    fn the_minimum_or_newer_on_windows_needs_nothing() {
        for output in [
            "2.1.139 (Claude Code)",
            "2.1.140 (Claude Code)",
            "2.2.0 (Claude Code)",
            "3.0.0 (Claude Code)",
        ] {
            assert_eq!(claude_step(output, true), None, "{output:?}");
        }
    }

    #[test]
    fn an_unreadable_version_on_windows_is_a_note_quoting_the_output() {
        assert_eq!(
            claude_step("command not found\n", true),
            Some(ClaudeStep::Note(
                "Cannot read Claude Code's version from `claude --version` (it printed: command not found); user-memory's Windows hooks need Claude Code 2.1.139 or later: run `claude update` if it is older".into()
            ))
        );
    }

    #[test]
    fn an_empty_output_on_windows_is_a_note_without_a_quote() {
        assert_eq!(
            claude_step("\r\n", true),
            Some(ClaudeStep::Note(
                "Cannot read Claude Code's version from `claude --version`; user-memory's Windows hooks need Claude Code 2.1.139 or later: run `claude update` if it is older".into()
            ))
        );
    }

    #[test]
    fn the_note_quotes_one_short_line_without_control_characters() {
        let long = format!("\u{1b}[31m{}", "x".repeat(QUOTED_CHARS + 20));
        let Some(ClaudeStep::Note(note)) = claude_step(&long, true) else {
            panic!("an unreadable version is a note");
        };
        let quoted = format!("(it printed: [31m{}…)", "x".repeat(QUOTED_CHARS - 4));
        assert!(note.contains(&quoted), "{note}");
        assert!(!note.contains('\u{1b}'), "{note}");
    }

    #[test]
    fn other_platforms_are_never_told_about_claude_code_versions() {
        for output in ["2.1.138 (Claude Code)", "", "command not found"] {
            assert_eq!(claude_step(output, false), None, "{output:?}");
        }
    }
}
