//! The clients' own instruction files that hold the user's global rules:
//! Claude Code's `rules/user-memory.md`, a whole file user-memory owns, and a
//! marked block at the end of Codex's global AGENTS file, whose other text
//! stays byte for byte. Writes are atomic and skipped when nothing changed,
//! so editors and file watchers see no churn.

use super::hooks::config_dir;
use crate::memory::{GENERATED, RULES_MAX_BYTES};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Numbers this process's writes, naming each one's temporary file.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Opens the block user-memory owns in Codex's AGENTS file.
const START: &str = "<!-- user-memory:start -->";
/// Closes it.
const END: &str = "<!-- user-memory:end -->";

/// Claude Code's personal rules file for user-memory, which it loads in
/// every project.
pub fn claude_file() -> Result<PathBuf> {
    Ok(config_dir("CLAUDE_CONFIG_DIR", ".claude")?
        .join("rules")
        .join("user-memory.md"))
}

/// Codex's global instructions: AGENTS.override.md when it has content,
/// since Codex then reads only that file, else AGENTS.md. Codex reads the
/// first non-empty file, so an empty override does not hide AGENTS.md.
pub fn codex_file() -> Result<PathBuf> {
    Ok(codex_file_in(&config_dir("CODEX_HOME", ".codex")?))
}

fn codex_file_in(home: &Path) -> PathBuf {
    let replacement = home.join("AGENTS.override.md");
    let has_content =
        fs::read(&replacement).is_ok_and(|text| text.iter().any(|b| !b.is_ascii_whitespace()));
    if has_content {
        replacement
    } else {
        home.join("AGENTS.md")
    }
}

/// What a write did.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Written,
    Unchanged,
}

/// How a file compares with the rules it should hold.
#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Current,
    Outdated,
    Missing,
}

/// The file's text, read through a symbolic link, or `None` when there is
/// none.
fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Cannot read {}", path.display())),
    }
}

/// Refuses a Claude rules file someone other than user-memory wrote.
fn owned(path: &Path, text: &str) -> Result<()> {
    if text.lines().next() != Some(GENERATED) {
        bail!(
            "{} was not written by user-memory; move it aside, or run setup with --no-instructions",
            path.display()
        );
    }
    Ok(())
}

/// Writes `text` as the Claude rules file.
pub fn write_claude(path: &Path, text: &str) -> Result<Change> {
    update(path, |existing| {
        if let Some(existing) = existing {
            owned(path, existing)?;
        }
        Ok(Next::Text(text.to_owned()))
    })
}

/// Removes the Claude rules file if user-memory wrote it; one the user
/// wrote holds nothing of user-memory's and stays.
pub fn remove_claude(path: &Path) -> Result<bool> {
    let change = update(path, |existing| {
        Ok(match existing {
            Some(text) if owned(path, text).is_ok() => Next::Remove,
            _ => Next::Keep,
        })
    })?;
    Ok(change == Change::Written)
}

/// Whether `path` is a symbolic link, which user-memory never writes
/// through: it usually points into a dotfiles repository, often public.
pub fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// Says why nothing was written through the link at `path`.
fn linked(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{} is a symbolic link; user-memory leaves it and its target as they are, so the rules were not written there; replace it with a regular file, or run setup with --no-instructions",
        path.display()
    )
}

pub fn claude_state(path: &Path, text: &str) -> Result<State> {
    Ok(match read(path)? {
        None => State::Missing,
        Some(existing) => {
            owned(path, &existing)?;
            if existing == text {
                State::Current
            } else {
                State::Outdated
            }
        }
    })
}

/// Where the block sits in `text`: from its start marker to just past its
/// end marker, or `None` when there is none. Refuses markers that do not
/// pair up, rather than guessing which text is ours.
fn block(path: &Path, text: &str) -> Result<Option<(usize, usize)>> {
    let (starts, ends) = (text.matches(START).count(), text.matches(END).count());
    match (starts, ends, text.find(START), text.find(END)) {
        (0, 0, _, _) => Ok(None),
        (1, 1, Some(start), Some(end)) if start < end => Ok(Some((start, end + END.len()))),
        _ => bail!(
            "{} has unbalanced user-memory markers; fix them by hand (one {START} line, then one {END} line), or remove both",
            path.display()
        ),
    }
}

/// The marked block holding `rules`.
fn marked(rules: &str) -> String {
    format!("{START}\n{rules}{END}")
}

/// `original` with the block holding `rules`: replaced in place when it is
/// there, else added at the end after a blank line. Text outside the
/// markers is kept as it is.
fn with_block(path: &Path, original: &str, rules: &str) -> Result<String> {
    Ok(match block(path, original)? {
        Some((start, end)) => format!(
            "{}{}{}",
            &original[..start],
            marked(rules),
            &original[end..]
        ),
        None if original.is_empty() => format!("{}\n", marked(rules)),
        None => format!("{original}\n{}\n", marked(rules)),
    })
}

/// `original` without the block, the newline that ends it, and the
/// separator added before it: the file as it was before the block came,
/// never with two of the user's lines joined.
fn without_block(path: &Path, original: &str) -> Result<Option<String>> {
    let Some((mut start, mut end)) = block(path, original)? else {
        return Ok(None);
    };
    if original[end..].starts_with('\n') {
        end += 1;
    }
    if start > 0 && original[..start].ends_with('\n') {
        start -= 1;
    }
    let (before, after) = (&original[..start], &original[end..]);
    // The newline taken with the block may have ended the user's own line:
    // put one back rather than join that line to the next.
    let join = if before.is_empty() || before.ends_with('\n') || after.is_empty() {
        ""
    } else {
        "\n"
    };
    Ok(Some(format!("{before}{join}{after}")))
}

/// Writes the block holding `rules` into the Codex file at `path`.
pub fn write_codex(path: &Path, rules: &str) -> Result<Change> {
    update(path, |existing| {
        Ok(Next::Text(with_block(
            path,
            existing.unwrap_or_default(),
            rules,
        )?))
    })
}

/// Removes the block from the Codex file at `path`, and the file when
/// nothing else is left in it.
pub fn remove_codex(path: &Path) -> Result<bool> {
    let change = update(path, |existing| {
        Ok(
            match existing
                .map(|text| without_block(path, text))
                .transpose()?
                .flatten()
            {
                None => Next::Keep,
                Some(rest) if rest.is_empty() => Next::Remove,
                Some(rest) => Next::Text(rest),
            },
        )
    })?;
    Ok(change == Change::Written)
}

pub fn codex_state(path: &Path, rules: &str) -> Result<State> {
    let Some(original) = read(path)? else {
        return Ok(State::Missing);
    };
    Ok(match block(path, &original)? {
        None => State::Missing,
        Some((start, end)) if original[start..end] == marked(rules) => State::Current,
        Some(_) => State::Outdated,
    })
}

/// What a write leaves in place of a file's current text.
enum Next {
    Text(String),
    Remove,
    Keep,
}

/// Tries to replace a file before giving up on it: about a tenth of a
/// second in all, longer than an editor takes to save, short enough not to
/// hold up a tool call.
const ATTEMPTS: u32 = 20;
/// Waits between tries, for a save under way to finish.
const PAUSE: std::time::Duration = std::time::Duration::from_millis(5);
/// Age past which a temporary file of ours is left over from a write that
/// was cut off (a session hook exits on time): a write takes milliseconds.
const STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// The start of the names of `path`'s temporary files.
fn temporary_prefix(path: &Path) -> Option<String> {
    Some(format!(
        ".{}.user-memory-",
        path.file_name()?.to_string_lossy()
    ))
}

/// Removes temporary files of ours beside `path` older than [`STALE`].
fn clear_stale(path: &Path) {
    let (Some(directory), Some(prefix)) = (path.parent(), temporary_prefix(path)) else {
        return;
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if stale && entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Replaces `path` with what `next` makes of its current text, through a
/// temporary file in the same directory that keeps the file's permissions.
/// The user may be saving the file in place at the same time: the text is
/// read again just before the rename, and when it changed, or reads empty as
/// in the middle of such a save, the write starts over from a fresh read.
/// After [`ATTEMPTS`] the file is left as it is; an empty file is written
/// only when every try found it empty.
fn update(path: &Path, next: impl Fn(Option<&str>) -> Result<Next>) -> Result<Change> {
    update_reading(path, read, next)
}

/// [`update`] with the file read by `read`, so a test can play a save in
/// progress exactly.
fn update_reading(
    path: &Path,
    read: impl Fn(&Path) -> Result<Option<String>>,
    next: impl Fn(Option<&str>) -> Result<Next>,
) -> Result<Change> {
    clear_stale(path);
    // Whether any try found text: an empty read after that is a save in the
    // middle of truncating, never the file's real content.
    let mut had_text = false;
    for attempt in 1..=ATTEMPTS {
        let existing = read(path)?;
        let empty = existing.as_deref() == Some("");
        had_text |= existing.as_deref().is_some_and(|text| !text.is_empty());
        if empty && (had_text || attempt < ATTEMPTS) {
            std::thread::sleep(PAUSE);
            continue;
        }
        let next = next(existing.as_deref())?;
        if matches!(&next, Next::Text(text) if existing.as_deref() == Some(text.as_str())) {
            return Ok(Change::Unchanged);
        }
        if !matches!(next, Next::Keep) && is_link(path) {
            return Err(linked(path));
        }
        let text = match next {
            Next::Keep => return Ok(Change::Unchanged),
            Next::Text(text) => text,
            Next::Remove => {
                if read(path)? != existing {
                    std::thread::sleep(PAUSE);
                    continue;
                }
                fs::remove_file(path)
                    .with_context(|| format!("Cannot remove {}", path.display()))?;
                return Ok(Change::Written);
            }
        };
        let directory = path.parent().context("Invalid instructions path")?;
        fs::create_dir_all(directory)
            .with_context(|| format!("Cannot create {}", directory.display()))?;
        let prefix = temporary_prefix(path).context("Invalid instructions path")?;
        // Unique per write, and created only if new: two writes never share
        // or truncate each other's temporary file.
        let temporary = directory.join(format!(
            "{prefix}{}-{}",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| -> Result<bool> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            if existing.is_some() {
                fs::set_permissions(&temporary, fs::metadata(path)?.permissions())?;
            }
            file.sync_all()?;
            if read(path)? != existing {
                return Ok(false);
            }
            fs::rename(&temporary, path)?;
            Ok(true)
        })();
        match written {
            Ok(true) => return Ok(Change::Written),
            Ok(false) => {
                let _ = fs::remove_file(&temporary);
                std::thread::sleep(PAUSE);
            }
            Err(err) => {
                let _ = fs::remove_file(&temporary);
                return Err(err.context(format!("Cannot write {}", path.display())));
            }
        }
    }
    bail!(
        "{} kept changing while user-memory wrote the rules, so it was left as it is; the next sync tries again",
        path.display()
    )
}

/// A client's instruction file that holds the rules.
pub enum Place {
    Claude(PathBuf),
    Codex(PathBuf),
}

impl Place {
    /// The places for the clients that are set up.
    pub fn for_clients(claude: bool, codex: bool) -> Result<Vec<Place>> {
        let mut places = Vec::new();
        if claude {
            places.push(Place::Claude(claude_file()?));
        }
        if codex {
            places.push(Place::Codex(codex_file()?));
        }
        Ok(places)
    }
    pub fn path(&self) -> &Path {
        match self {
            Self::Claude(path) | Self::Codex(path) => path,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Claude(_) => "Claude Code",
            Self::Codex(_) => "Codex",
        }
    }
    pub fn write(&self, rules: &str) -> Result<Change> {
        match self {
            Self::Claude(path) => write_claude(path, rules),
            Self::Codex(path) => write_codex(path, rules),
        }
    }
    /// Most bytes the rules may take here. Codex reads all its instruction
    /// files up to one limit, so the user's own text in its file leaves the
    /// rules less room; Claude Code has no such limit.
    pub fn room(&self) -> usize {
        match self {
            Self::Claude(_) => RULES_MAX_BYTES,
            Self::Codex(path) => codex_room(path),
        }
    }
    pub fn state(&self, rules: &str) -> Result<State> {
        match self {
            Self::Claude(path) => claude_state(path, rules),
            Self::Codex(path) => codex_state(path, rules),
        }
    }
}

/// The most the rules may take in the Codex file at `path`: their usual room
/// less the user's own text there and the marker lines around the block.
fn codex_room(path: &Path) -> usize {
    let text = read(path).ok().flatten().unwrap_or_default();
    let own = without_block(path, &text)
        .ok()
        .flatten()
        .map_or(text.len(), |rest| rest.len());
    // The two marker lines and the newlines around them.
    let markers = START.len() + END.len() + 3;
    RULES_MAX_BYTES.saturating_sub(own + markers)
}

/// The room for the rules in all of `places`: the smallest, since both
/// files hold the same text.
pub fn room(places: &[Place]) -> usize {
    places
        .iter()
        .map(Place::room)
        .min()
        .unwrap_or(RULES_MAX_BYTES)
}

/// Removes the rules from every place setup may have put them: the Claude
/// rules file and the block in either Codex file. Returns what it removed.
pub fn remove_all() -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let claude = claude_file()?;
    if remove_claude(&claude)? {
        removed.push(claude);
    }
    let home = config_dir("CODEX_HOME", ".codex")?;
    for name in ["AGENTS.override.md", "AGENTS.md"] {
        let path = home.join(name);
        if remove_codex(&path)? {
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Every place setup may have written the rules that holds them now: the
/// Claude rules file, and either Codex file with the marked block.
pub fn holding_rules() -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let claude = claude_file()?;
    if let Some(text) = read(&claude)?
        && owned(&claude, &text).is_ok()
    {
        found.push(claude);
    }
    let home = config_dir("CODEX_HOME", ".codex")?;
    for name in ["AGENTS.override.md", "AGENTS.md"] {
        let path = home.join(name);
        if let Some(text) = read(&path)?
            && block(&path, &text)?.is_some()
        {
            found.push(path);
        }
    }
    Ok(found)
}

/// Removes the rules from `path`, one of [`holding_rules`]: the whole
/// Claude rules file, or only the block in a Codex file.
pub fn remove_rules(path: &Path) -> Result<bool> {
    if path == claude_file()? {
        remove_claude(path)
    } else {
        remove_codex(path)
    }
}

/// Checks that `text` is the rules view, so an error or another server's
/// reply never lands in the user's instruction files.
pub fn checked(text: String) -> Result<String> {
    if !text.starts_with(GENERATED) {
        bail!(
            "The memory server did not return the rules view: {}",
            text.lines().next().unwrap_or_default()
        );
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(line: &str) -> String {
        format!("{GENERATED}\n# rules\n- {line}\n")
    }

    #[test]
    fn the_claude_file_is_created_rewritten_and_left_alone_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules/user-memory.md");
        assert_eq!(claude_state(&path, &rules("a")).unwrap(), State::Missing);
        assert_eq!(write_claude(&path, &rules("a")).unwrap(), Change::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), rules("a"));
        let written = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(write_claude(&path, &rules("a")).unwrap(), Change::Unchanged);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
        assert_eq!(claude_state(&path, &rules("a")).unwrap(), State::Current);
        assert_eq!(claude_state(&path, &rules("b")).unwrap(), State::Outdated);
        // The rewrite keeps the file's mode, where files have one.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(write_claude(&path, &rules("b")).unwrap(), Change::Written);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), rules("b"));
        // No temporary file is left beside it.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert!(remove_claude(&path).unwrap());
        assert!(!path.exists());
        assert!(!remove_claude(&path).unwrap());
    }

    #[test]
    fn a_claude_file_the_user_wrote_is_never_replaced_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user-memory.md");
        fs::write(&path, "# My own rules\n").unwrap();
        for err in [
            write_claude(&path, &rules("a")).unwrap_err(),
            claude_state(&path, &rules("a")).unwrap_err(),
        ] {
            assert!(
                err.to_string().contains("was not written by user-memory"),
                "{err}"
            );
        }
        // Nothing of user-memory's is there to remove.
        assert!(!remove_claude(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "# My own rules\n");
    }

    #[test]
    fn the_codex_block_goes_at_the_end_and_leaves_other_text_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        for original in [
            "",
            "# Mine\r\nKeep CRLF lines\r\n@RTK.md",
            "# Mine\nwith a trailing newline\n",
        ] {
            if original.is_empty() {
                let _ = fs::remove_file(&path);
            } else {
                fs::write(&path, original).unwrap();
            }
            assert_eq!(write_codex(&path, &rules("a")).unwrap(), Change::Written);
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.starts_with(original), "{text:?}");
            if original.is_empty() {
                // A new file holds the block alone, with no blank line before it.
                assert_eq!(text, format!("{}\n", marked(&rules("a"))));
            }
            assert!(
                text.ends_with(&format!("{}\n", marked(&rules("a")))),
                "{text:?}"
            );
            assert_eq!(codex_state(&path, &rules("a")).unwrap(), State::Current);
            assert_eq!(codex_state(&path, &rules("b")).unwrap(), State::Outdated);
            assert_eq!(write_codex(&path, &rules("a")).unwrap(), Change::Unchanged);
            // Text added after the block by the user stays where it is.
            let edited = format!("{text}after\n");
            fs::write(&path, &edited).unwrap();
            write_codex(&path, &rules("b")).unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                edited.replace(&marked(&rules("a")), &marked(&rules("b")))
            );
            fs::write(&path, &text).unwrap();
            assert!(remove_codex(&path).unwrap());
            if original.is_empty() {
                assert!(!path.exists());
            } else {
                assert_eq!(fs::read_to_string(&path).unwrap(), original);
            }
        }
        assert!(!remove_codex(&path).unwrap());
    }

    #[test]
    fn removing_the_block_never_joins_the_users_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let block = format!("{}\n", marked(&rules("a")));
        // Created in an empty file, then the user added a line on each side.
        // Added after a file with no final newline, then a line below.
        for (around, expected) in [
            (
                format!("My line added later\n{block}And one below\n"),
                "My line added later\nAnd one below\n",
            ),
            (format!("abc\n{block}xyz"), "abc\nxyz"),
            // The usual blank-line separator, with the user's text after.
            (format!("abc\n\n{block}xyz\n"), "abc\nxyz\n"),
            // A block the user moved onto the end of a line of theirs.
            (format!("abc {block}"), "abc "),
        ] {
            fs::write(&path, &around).unwrap();
            assert!(remove_codex(&path).unwrap());
            assert_eq!(fs::read_to_string(&path).unwrap(), expected, "{around:?}");
        }
    }

    #[test]
    fn the_users_own_codex_text_leaves_the_rules_less_room() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let markers = START.len() + END.len() + 3;
        assert_eq!(codex_room(&path), RULES_MAX_BYTES - markers);
        let own = "x".repeat(20 * 1024);
        fs::write(&path, &own).unwrap();
        assert_eq!(codex_room(&path), RULES_MAX_BYTES - own.len() - markers);
        // The block already there is not the user's text.
        write_codex(&path, &rules("a")).unwrap();
        assert_eq!(codex_room(&path), RULES_MAX_BYTES - own.len() - markers);
        fs::write(&path, "x".repeat(40 * 1024)).unwrap();
        assert_eq!(codex_room(&path), 0);
        let places = [
            Place::Claude(dir.path().join("rules.md")),
            Place::Codex(path),
        ];
        assert_eq!(room(&places), 0);
        assert_eq!(room(&places[..1]), RULES_MAX_BYTES);
    }

    #[test]
    fn a_save_in_place_during_a_write_keeps_the_users_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let original: String = (0..50).map(|i| format!("Original line {i}\n")).collect();
        fs::write(&path, &original).unwrap();
        /// Saves the simulated editor makes.
        const EDITS: usize = 400;
        // An editor that keeps the file's inode: it reads the file, then
        // truncates it and writes it back with one more line, every
        // millisecond or so.
        let editor = {
            let path = path.clone();
            std::thread::spawn(move || {
                for i in 0..EDITS {
                    let current = fs::read_to_string(&path).unwrap_or_default();
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&path)
                        .unwrap();
                    file.write_all(format!("Edit {i}\n{current}").as_bytes())
                        .unwrap();
                    drop(file);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            })
        };
        let mut round = 0;
        while !editor.is_finished() {
            let _ = write_codex(&path, &rules(&round.to_string()));
            round += 1;
        }
        editor.join().unwrap();
        write_codex(&path, &rules("last")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for i in 0..50 {
            assert!(
                text.contains(&format!("Original line {i}\n")),
                "line {i} lost after {round} writes"
            );
        }
        let lost = (0..EDITS)
            .filter(|i| !text.contains(&format!("Edit {i}\n")))
            .count();
        // A save landing between the last check and the rename can still be
        // lost: no lock-free write closes that window, and editors take no
        // locks.
        assert!(
            lost <= EDITS / 20,
            "{lost} of {EDITS} edits lost in {round} writes"
        );
        assert_eq!(text.matches(START).count(), 1);
        assert_eq!(text.matches(END).count(), 1);
    }

    #[test]
    fn an_empty_file_is_written_only_once_every_try_found_it_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        // Truly empty: every try reads it so, and the last one writes.
        fs::write(&path, "").unwrap();
        let started = std::time::Instant::now();
        assert_eq!(write_codex(&path, &rules("a")).unwrap(), Change::Written);
        assert!(
            started.elapsed() >= PAUSE * (ATTEMPTS - 1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\n", marked(&rules("a")))
        );
        // Text first, then empty on every later try, as a save that keeps
        // truncating the file: never written, the text stays.
        fs::write(&path, "# Mine\n").unwrap();
        let reads = std::cell::Cell::new(0);
        let saving = |path: &Path| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                read(path)
            } else {
                Ok(Some(String::new()))
            }
        };
        let err = update_reading(&path, saving, |existing| {
            Ok(Next::Text(with_block(
                &path,
                existing.unwrap_or_default(),
                &rules("a"),
            )?))
        })
        .unwrap_err();
        assert!(err.to_string().contains("kept changing"), "{err}");
        // Each try reads once, and the first, finding text, reads again
        // before its rename.
        assert_eq!(reads.get(), ATTEMPTS as usize + 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "# Mine\n");
    }

    #[test]
    fn a_write_clears_temporary_files_left_by_an_interrupted_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let stale = dir.path().join(".AGENTS.md.user-memory-99999-0");
        let fresh = dir.path().join(".AGENTS.md.user-memory-99999-1");
        let other = dir.path().join(".AGENTS.md.swp");
        for file in [&stale, &fresh, &other] {
            fs::write(file, "partial").unwrap();
        }
        let old = std::time::SystemTime::now() - STALE * 2;
        for file in [&stale, &other] {
            fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        write_codex(&path, &rules("a")).unwrap();
        assert!(!stale.exists());
        // One that may still be in use, and the user's own files, stay.
        assert!(fresh.exists());
        assert!(other.exists());
    }

    #[test]
    fn unbalanced_codex_markers_are_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        for broken in [
            format!("{START}\nonly a start\n"),
            format!("only an end\n{END}\n"),
            format!("{END}\nreversed\n{START}\n"),
            format!("{START}\n{START}\n{END}\n"),
        ] {
            fs::write(&path, &broken).unwrap();
            for err in [
                write_codex(&path, &rules("a")).unwrap_err(),
                remove_codex(&path).unwrap_err(),
                codex_state(&path, &rules("a")).unwrap_err(),
            ] {
                assert!(
                    err.to_string().contains("unbalanced user-memory markers"),
                    "{err}"
                );
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), broken);
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be.
        let path = dir.path().join("user-memory.md");
        fs::create_dir(&path).unwrap();
        assert!(claude_state(&path, &rules("a")).is_err());
        assert!(codex_state(&path, &rules("a")).is_err());
        assert!(write_claude(&path, &rules("a")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_refused_and_left_with_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/AGENTS.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "# Kept in a dotfiles repository\n").unwrap();
        let link = dir.path().join("AGENTS.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(is_link(&link));
        // Writes through a link are refused, even into a file that is ours.
        let ours = dir.path().join("dotfiles/user-memory.md");
        fs::write(&ours, rules("a")).unwrap();
        let claude = dir.path().join("user-memory.md");
        std::os::unix::fs::symlink(&ours, &claude).unwrap();
        for err in [
            write_codex(&link, &rules("a")).unwrap_err(),
            write_claude(&claude, &rules("b")).unwrap_err(),
            remove_claude(&claude).unwrap_err(),
        ] {
            assert!(err.to_string().contains("is a symbolic link"), "{err}");
            assert!(err.to_string().contains("not written"), "{err}");
        }
        // Reading through it is harmless: with no block there is nothing to
        // remove, and the state says so.
        assert!(!remove_codex(&link).unwrap());
        assert!(!remove_claude(&link).unwrap());
        assert_eq!(fs::read_to_string(&ours).unwrap(), rules("a"));
        assert_eq!(codex_state(&link, &rules("a")).unwrap(), State::Missing);
        // A block that got there some other way is reported, and kept.
        let with = format!(
            "# Kept in a dotfiles repository\n\n{}\n",
            marked(&rules("a"))
        );
        fs::write(&target, &with).unwrap();
        assert_eq!(codex_state(&link, &rules("a")).unwrap(), State::Current);
        assert_eq!(write_codex(&link, &rules("a")).unwrap(), Change::Unchanged);
        let err = remove_codex(&link).unwrap_err();
        assert!(err.to_string().contains("is a symbolic link"), "{err}");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read_to_string(&target).unwrap(), with);
    }

    #[test]
    fn codex_reads_the_override_file_when_there_is_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(codex_file_in(dir.path()), dir.path().join("AGENTS.md"));
        // Codex reads the first non-empty file, so an empty override is
        // passed over, and the rules go where Codex looks.
        for empty in ["", " \n\t\n"] {
            fs::write(dir.path().join("AGENTS.override.md"), empty).unwrap();
            assert_eq!(
                codex_file_in(dir.path()),
                dir.path().join("AGENTS.md"),
                "{empty:?}"
            );
        }
        fs::write(dir.path().join("AGENTS.override.md"), "# override\n").unwrap();
        assert_eq!(
            codex_file_in(dir.path()),
            dir.path().join("AGENTS.override.md")
        );
    }
}
