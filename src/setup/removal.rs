//! What `uninstall` and `server remove` share: the Removed and Left lines,
//! the user's choice about the memories, settled before anything is
//! removed, and the terminal the questions go to.

use anyhow::{Result, bail};
use rusqlite::Connection;
use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};

/// How a private file is described to the user after it is written.
pub(crate) const PRIVATE: &str = if cfg!(windows) {
    "readable only by you"
} else {
    "mode 0600"
};

/// What was printed, so the run can end with the right status.
#[derive(Default)]
pub(crate) struct Report {
    pub(crate) dry_run: bool,
    pub(crate) changed: usize,
    /// Items of ours that could not be removed, each with its command.
    pub(crate) left: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl Report {
    pub(crate) fn removed(&mut self, what: impl AsRef<str>) {
        self.changed += 1;
        let verb = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        println!("  {verb}: {}", what.as_ref());
    }
    pub(crate) fn left(&mut self, what: String) {
        println!("  Left: {what}");
        self.left.push(what);
    }
    pub(crate) fn note(&mut self, what: String) {
        self.notes.push(what);
    }
}

/// The user's answer about the memories, settled before anything is
/// removed so a refused purge changes nothing.
pub(crate) enum Memories {
    Keep,
    /// Delete after exporting them, unless `export` is false.
    Delete {
        export: bool,
    },
    /// Ask on the terminal when the memories step comes, offering an
    /// export unless --no-export was given.
    Ask {
        terminal: Terminal,
        export: bool,
    },
}

impl Memories {
    /// The choice the options and the terminal allow: --purge --yes
    /// deletes, a terminal asks, and a dry run or no terminal keeps. A
    /// --purge without --yes and without a terminal is refused, so nothing
    /// is deleted that the user did not confirm.
    pub(crate) fn choose(purge: bool, yes: bool, no_export: bool, dry_run: bool) -> Result<Self> {
        Ok(match (purge, yes, Terminal::open()) {
            (true, true, _) => Self::Delete { export: !no_export },
            (_, _, Some(terminal)) if !dry_run => Self::Ask {
                terminal,
                export: !no_export,
            },
            (true, false, None) if !dry_run => bail!(
                "--purge deletes your memories; without a terminal to confirm, it also needs --yes. Nothing was changed."
            ),
            _ => Self::Keep,
        })
    }

    /// Whether to delete the memories at `what`, as described to the user,
    /// and whether to export them first; asks when the choice is to ask.
    pub(crate) fn decide(self, what: &str) -> Result<Option<bool>> {
        match self {
            Self::Keep => Ok(None),
            Self::Delete { export } => Ok(Some(export)),
            Self::Ask {
                mut terminal,
                export,
            } => {
                if terminal.ask(&format!("Keep your memories ({what})? [Y/n] "), true)? {
                    return Ok(None);
                }
                let export = export
                    && terminal.ask("Export them to a file in this folder first? [Y/n] ", true)?;
                if terminal.line("Type delete to delete them for good: ")? != "delete" {
                    return Ok(None);
                }
                Ok(Some(export))
            }
        }
    }
}

/// Where an export taken before deleting goes: a file in the current
/// folder named for the time, in UTC.
pub(crate) fn export_path() -> Result<PathBuf> {
    // SQLite is already here, and its clock formats UTC on every platform.
    let stamp: String = Connection::open_in_memory()?.query_row(
        "SELECT strftime('%Y%m%dT%H%M%SZ','now')",
        [],
        |row| row.get(0),
    )?;
    Ok(env::current_dir()?.join(format!("user-memory-export-{stamp}.json")))
}

/// The terminal's input and output devices: the controlling terminal on
/// Unix; on Windows the console's own, which stay the console when
/// standard input is the piped installer script.
const TERMINAL: (&str, &str) = if cfg!(windows) {
    ("CONIN$", "CONOUT$")
} else {
    ("/dev/tty", "/dev/tty")
};

/// The terminal, for questions even when standard input is the piped
/// installer script.
pub(crate) struct Terminal {
    reader: BufReader<fs::File>,
    writer: fs::File,
}

impl Terminal {
    /// The terminal, if this process has one someone can answer on: none
    /// without a controlling terminal on Unix, or on Windows without a
    /// console window, whose CONIN$ opens but no one types in.
    fn open() -> Option<Self> {
        #[cfg(windows)]
        if !crate::platform::windows::console_window() {
            return None;
        }
        let (input, output) = TERMINAL;
        let reader = fs::File::open(input).ok()?;
        let writer = fs::OpenOptions::new().write(true).open(output).ok()?;
        Some(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }
    fn line(&mut self, prompt: &str) -> Result<String> {
        self.writer.write_all(prompt.as_bytes())?;
        self.writer.flush()?;
        let mut answer = String::new();
        self.reader.read_line(&mut answer)?;
        Ok(answer.trim().to_owned())
    }
    /// A yes or no question; an empty answer takes `default`.
    fn ask(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let answer = self.line(prompt)?.to_lowercase();
        Ok(match answer.as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_is_named_for_the_time_in_utc() {
        let path = export_path().unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let stamp = name
            .strip_prefix("user-memory-export-")
            .and_then(|rest| rest.strip_suffix(".json"))
            .unwrap();
        assert_eq!(stamp.len(), "20261018T120000Z".len(), "{name}");
        assert!(
            stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T',
            "{name}"
        );
        assert_eq!(path.parent(), Some(env::current_dir().unwrap().as_path()));
    }
}
