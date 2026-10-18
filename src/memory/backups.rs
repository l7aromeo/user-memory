//! Automatic daily backups of a store, rotated in a `backups` folder beside
//! it. A write makes one after its turn has passed on, so other writers
//! never wait for it, and a failed backup never fails the write.

use super::migrate::{copy_into, create_private, memory_rows};
use crate::{private, shell};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

/// Automatic backups kept per store; once a new one is written, older ones
/// are deleted. A week of daily copies.
pub const KEEP: usize = 7;

/// The folder beside the store that holds its automatic backups.
pub const FOLDER: &str = "backups";

/// A write makes a backup when the newest is at least this old, so a store
/// written every day gets one a day.
const DUE: Duration = Duration::from_secs(24 * 60 * 60);

/// Doctor asks for a backup once the newest is older than this while
/// memories changed since: two days of writes without one.
const STALE: Duration = Duration::from_secs(48 * 60 * 60);

/// How long this process lets writes go without trying again after a backup
/// failed, so a folder it cannot write does not cost every write a try.
const RETRY: Duration = Duration::from_secs(60 * 60);

/// The file in the folder saying why the last backup failed; the next
/// backup that succeeds removes it.
const FAILURE: &str = "last-failure";

/// Marks a copy still being written, `.<name>.partial-<process id>`, so a
/// copy cut short never looks like a backup.
const PARTIAL: &str = ".partial-";

/// One automatic backup.
pub struct Backup {
    pub path: PathBuf,
    /// The UTC date in its name, as `YYYY-MM-DD`.
    pub date: String,
    pub modified: SystemTime,
    pub bytes: u64,
}

impl Backup {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// A backup just written, and the older ones deleted to keep [`KEEP`].
pub struct Made {
    pub backup: Backup,
    pub memories: i64,
    pub deleted: Vec<String>,
}

/// The folder holding `db`'s automatic backups.
pub fn folder(db: &Path) -> PathBuf {
    db.with_file_name(FOLDER)
}

/// How backups of one store are named: the store's name with the date
/// before its extension, `memories.sqlite3` giving `memories-<date>.sqlite3`.
struct Names {
    stem: String,
    extension: Option<String>,
}

impl Names {
    fn of(db: &Path) -> Result<Self> {
        let stem = db
            .file_stem()
            .context("Invalid memory store path")?
            .to_string_lossy()
            .into_owned();
        let extension = db
            .extension()
            .map(|extension| extension.to_string_lossy().into_owned());
        Ok(Self { stem, extension })
    }

    fn name(&self, date: &str) -> String {
        match &self.extension {
            Some(extension) => format!("{}-{date}.{extension}", self.stem),
            None => format!("{}-{date}", self.stem),
        }
    }

    /// The date in `file` when it names one of these backups.
    fn date<'f>(&self, file: &'f str) -> Option<&'f str> {
        let rest = file.strip_prefix(&self.stem)?.strip_prefix('-')?;
        let date = match &self.extension {
            Some(extension) => rest.strip_suffix(extension)?.strip_suffix('.')?,
            None => rest,
        };
        is_date(date).then_some(date)
    }
}

/// Whether `text` has the shape `YYYY-MM-DD`.
fn is_date(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(at, byte)| match at {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// `db`'s automatic backups, newest first; none when the folder is missing.
pub fn list(db: &Path) -> Result<Vec<Backup>> {
    let names = Names::of(db)?;
    let folder = folder(db);
    let entries = match fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", folder.display())),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("Cannot read {}", folder.display()))?;
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(date) = names.date(&file) else {
            continue;
        };
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        backups.push(Backup {
            path: entry.path(),
            date: date.to_owned(),
            modified: metadata.modified()?,
            bytes: metadata.len(),
        });
    }
    backups.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(backups)
}

/// Every file of `db`'s automatic backups: the backups, a failure note and
/// copies cut short. Anything else in the folder is not user-memory's.
pub fn files(db: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = list(db)?.into_iter().map(|backup| backup.path).collect();
    let folder = folder(db);
    let failure = folder.join(FAILURE);
    if failure.is_file() {
        files.push(failure);
    }
    files.extend(partials(db, &folder)?);
    files.sort();
    Ok(files)
}

/// Copies of `db` cut short, such as by a process stopped mid-copy.
fn partials(db: &Path, folder: &Path) -> Result<Vec<PathBuf>> {
    let names = Names::of(db)?;
    let Ok(entries) = fs::read_dir(folder) else {
        return Ok(Vec::new());
    };
    Ok(entries
        .flatten()
        .filter(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            file.strip_prefix('.')
                .and_then(|file| file.split_once(PARTIAL))
                .is_some_and(|(name, _)| names.date(name).is_some())
        })
        .map(|entry| entry.path())
        .collect())
}

/// This process's backups under way, and when each store's last backup in
/// this process failed. Locked only to read or change these, never during
/// a copy, so writers finishing while one is made do not wait for it.
#[derive(Default)]
struct State {
    copying: HashSet<PathBuf>,
    failed: HashMap<PathBuf, Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Runs `change` on [`STATE`] under its lock.
fn with_state<T>(change: impl FnOnce(&mut State) -> T) -> T {
    let mut state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    change(state.get_or_insert_with(State::default))
}

/// The key [`STATE`] has for the store `db`, however it was spelled: the
/// path the file system resolves (links followed, and on Windows long
/// names in place of short ones such as `RUNNER~1`), or `db` as given when
/// it cannot be resolved. A write passes the path SQLite reports and setup
/// the one it was configured with; both must find the same entry.
fn key(db: &Path) -> PathBuf {
    dunce::canonicalize(db).unwrap_or_else(|_| db.to_path_buf())
}

/// Marks a store's backup as under way in this process until it drops, even
/// when the copy panics.
struct Copying<'p>(&'p Path);

impl Drop for Copying<'_> {
    fn drop(&mut self) {
        with_state(|state| state.copying.remove(self.0));
    }
}

/// Makes the day's backup of `db` when one is due: after every write once
/// its turn has passed on, and when setup has readied the store, so an
/// upgraded store has one from the start. A writer that finds this
/// process already making one leaves it to that writer. Never fails; a
/// failure goes to stderr and to the folder's failure note, which doctor
/// reports.
pub fn back_up_if_due(db: &Path) {
    let key = key(db);
    let start = with_state(|state| {
        let retrying = state
            .failed
            .get(&key)
            .is_some_and(|when| when.elapsed() < RETRY);
        !retrying && state.copying.insert(key.clone())
    });
    if !start {
        return;
    }
    let _copying = Copying(&key);
    let due = match list(db) {
        Ok(backups) => backups
            .first()
            .is_none_or(|newest| age(newest.modified) >= DUE),
        Err(_) => true,
    };
    if !due {
        return;
    }
    // A store just created has had no memory written yet: nothing to keep.
    match has_memories(db).and_then(|has| has.then(|| make(db)).transpose()) {
        Ok(_) => {
            with_state(|state| state.failed.remove(&key));
        }
        Err(err) => {
            with_state(|state| state.failed.insert(key.clone(), Instant::now()));
            eprintln!(
                "user-memory: the automatic backup of {} failed: {err:#}",
                db.display()
            );
            note_failure(db, &format!("{err:#}"));
        }
    }
}

/// Makes a backup of `db` now, replacing today's if there is one.
pub fn now(db: &Path) -> Result<Made> {
    make(db)
}

/// Whether the store at `db` holds any memory.
fn has_memories(db: &Path) -> Result<bool> {
    let store = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    store.busy_timeout(super::schema::BUSY)?;
    Ok(memory_rows(&store)? > 0)
}

fn make(db: &Path) -> Result<Made> {
    if !db.is_file() {
        anyhow::bail!("No memory store at {}", db.display());
    }
    let names = Names::of(db)?;
    let folder = folder(db);
    create_folder(&folder)?;
    let date: String =
        Connection::open_in_memory()?.query_row("SELECT date('now')", [], |r| r.get(0))?;
    let name = names.name(&date);
    let path = folder.join(&name);
    let partial = folder.join(format!(".{name}{PARTIAL}{}", std::process::id()));
    // Left by an earlier process that had this id and was stopped mid-copy.
    let _ = fs::remove_file(&partial);
    create_private(&partial).with_context(|| format!("Cannot create {}", partial.display()))?;
    let copied = copy_into(db, &partial).and_then(|memories| {
        fs::rename(&partial, &path).with_context(|| format!("Cannot write {}", path.display()))?;
        Ok(memories)
    });
    let memories = match copied {
        Ok(memories) => memories,
        Err(err) => {
            let _ = fs::remove_file(&partial);
            return Err(err);
        }
    };
    let _ = fs::remove_file(folder.join(FAILURE));
    let deleted = rotate(db, &folder)?;
    let metadata = fs::metadata(&path)?;
    Ok(Made {
        backup: Backup {
            path,
            date,
            modified: metadata.modified()?,
            bytes: metadata.len(),
        },
        memories,
        deleted,
    })
}

/// Creates the backups folder, owner-only like the store's own folder.
fn create_folder(folder: &Path) -> Result<()> {
    private::create_dir(folder).with_context(|| format!("Cannot create {}", folder.display()))
}

/// Deletes the backups past the newest [`KEEP`], and copies cut short more
/// than a day ago; returns the backups' names. One that cannot be deleted,
/// or that another process rotating at the same time took, never fails the
/// backup just written; the next rotation tries it again.
fn rotate(db: &Path, folder: &Path) -> Result<Vec<String>> {
    let mut deleted = Vec::new();
    for old in list(db)?.into_iter().skip(KEEP) {
        if fs::remove_file(&old.path).is_ok() {
            deleted.push(old.name());
        }
    }
    for partial in partials(db, folder)? {
        if fs::metadata(&partial)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| age(modified) >= DUE)
        {
            let _ = fs::remove_file(partial);
        }
    }
    Ok(deleted)
}

/// Writes why the backup failed into the folder, for doctor.
fn note_failure(db: &Path, message: &str) {
    let folder = folder(db);
    let note = folder.join(FAILURE);
    let written = (|| -> Result<()> {
        create_folder(&folder)?;
        let when: String = Connection::open_in_memory()?.query_row(
            "SELECT strftime('%Y-%m-%d %H:%M UTC','now')",
            [],
            |r| r.get(0),
        )?;
        let _ = fs::remove_file(&note);
        use std::io::Write;
        writeln!(create_private(&note)?, "{when}: {message}")?;
        Ok(())
    })();
    if let Err(err) = written {
        eprintln!(
            "user-memory: cannot note the failure in {}: {err:#}",
            note.display()
        );
    }
}

fn age(time: SystemTime) -> Duration {
    SystemTime::now().duration_since(time).unwrap_or_default()
}

/// What doctor says about `db`'s automatic backups.
pub struct Status {
    folder: PathBuf,
    newest: Option<Backup>,
    kept: usize,
    /// Whether the store was written after the newest backup.
    changed_since: bool,
    has_memories: bool,
    /// The failure note's text.
    failure: Option<String>,
}

pub fn status(db: &Path) -> Result<Status> {
    let backups = list(db)?;
    let folder = folder(db);
    let failure = match fs::read_to_string(folder.join(FAILURE)) {
        Ok(text) => Some(text.trim().to_owned()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", folder.display())),
    };
    // A reader opening the store creates an empty log, which holds no write.
    let written = ["", "-wal"]
        .iter()
        .filter_map(|suffix| fs::metadata(suffixed(db, suffix)).ok())
        .filter(|metadata| metadata.len() > 0)
        .filter_map(|metadata| metadata.modified().ok())
        .max();
    let changed_since = match (written, backups.first()) {
        (Some(written), Some(newest)) => written > newest.modified,
        (written, None) => written.is_some(),
        (None, Some(_)) => false,
    };
    let has_memories = db.is_file() && has_memories(db)?;
    let kept = backups.len();
    Ok(Status {
        folder,
        newest: backups.into_iter().next(),
        kept,
        changed_since,
        has_memories,
        failure,
    })
}

impl Status {
    /// Doctor's line.
    pub fn line(&self) -> String {
        match &self.newest {
            Some(newest) => format!(
                "Automatic backups: newest {}, {} old ({} kept in {})",
                newest.name(),
                describe_age(age(newest.modified)),
                self.kept,
                self.folder.display()
            ),
            None => format!("Automatic backups: none yet in {}", self.folder.display()),
        }
    }

    /// Doctor's Action needed step, when the backups need the user.
    pub fn action(&self) -> Option<String> {
        const NOW: &str = "run `user-memory backup now`";
        if let Some(failure) = &self.failure {
            return Some(format!(
                "Automatic backups: the last one failed ({failure}); fix the cause, then {NOW}"
            ));
        }
        match &self.newest {
            None if self.has_memories => Some(format!("Automatic backups: none yet; {NOW}")),
            Some(newest) if age(newest.modified) > STALE && self.changed_since => Some(format!(
                "Automatic backups: the newest is {} old though memories changed since; {NOW}",
                describe_age(age(newest.modified))
            )),
            _ => None,
        }
    }
}

/// An age in whole minutes, hours or days, as "3 hours".
fn describe_age(age: Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let seconds = age.as_secs();
    let (count, unit) = if seconds < HOUR {
        (seconds / MINUTE, "minute")
    } else if seconds < 2 * DAY {
        (seconds / HOUR, "hour")
    } else {
        (seconds / DAY, "day")
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

/// `backup list`'s report: each backup newest first, then how to restore one.
pub fn describe(db: &Path) -> Result<String> {
    let backups = list(db)?;
    let folder = folder(db);
    if backups.is_empty() {
        return Ok(format!(
            "No automatic backups yet in {}. The first write of a day makes one; `user-memory backup now` makes one now.",
            folder.display()
        ));
    }
    let mut out = format!(
        "Automatic backups in {}, newest first (the newest {KEEP} are kept):\n",
        folder.display()
    );
    for backup in &backups {
        let when: String = Connection::open_in_memory()?.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%MZ',?1,'unixepoch')",
            [backup
                .modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()],
            |r| r.get(0),
        )?;
        let memories = Connection::open_with_flags(&backup.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(anyhow::Error::from)
            .and_then(|copy| Ok(memory_rows(&copy)?));
        let memories = memories.map_or_else(|_| "unreadable".to_owned(), count);
        out.push_str(&format!(
            "  {}  made {when}  {memories}  {}\n",
            backup.name(),
            describe_size(backup.bytes)
        ));
    }
    out.push_str(&restore(db, &backups[0].path, cfg!(windows)));
    Ok(out)
}

/// How to restore `backup` over the store `db`, for a POSIX shell or, on
/// Windows, for PowerShell. The log and shared-memory files belong to the
/// store being replaced: left beside the copy, SQLite would apply that
/// store's writes to it. Either may be missing, which is no error.
fn restore(db: &Path, backup: &Path, windows: bool) -> String {
    let (wal, shm) = (suffixed(db, "-wal"), suffixed(db, "-shm"));
    let intro = "To restore one, stop every client and server using the store, then run";
    if windows {
        let quote = shell::powershell_quote;
        format!(
            "{intro} in PowerShell, with the backup you choose:\n  Get-Item -Force -LiteralPath {}, {} -ErrorAction Ignore | Remove-Item -Force\n  Copy-Item -Force -LiteralPath {} -Destination {}",
            quote(&wal),
            quote(&shm),
            quote(backup),
            quote(db)
        )
    } else {
        let quote = shell::quote;
        format!(
            "{intro}, with the backup you choose:\n  rm -f {} {}\n  cp {} {}",
            quote(&wal),
            quote(&shm),
            quote(backup),
            quote(db)
        )
    }
}

/// `path` with `suffix` appended to its file name, as the store's log is.
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

/// A number of memories, as "1 memory" or "3 memories".
pub fn count(memories: i64) -> String {
    match memories {
        1 => "1 memory".to_owned(),
        _ => format!("{memories} memories"),
    }
}

/// A file size in KB or MB with one decimal.
fn describe_size(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    const MB: f64 = 1000.0 * KB;
    let bytes = bytes as f64;
    if bytes < MB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::super::schema::{connect, write};
    use super::*;
    use tempfile::tempdir;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    const MINUTE: Duration = Duration::from_secs(60);
    const HOUR: Duration = Duration::from_secs(60 * 60);

    /// A store at `dir`, holding no memory yet.
    fn store(dir: &Path) -> PathBuf {
        let db = dir.join("memories.sqlite3");
        connect(&db).unwrap();
        db
    }

    /// Records one memory through a write turn, as every writer does.
    fn add(db: &Path) {
        let conn = connect(db).unwrap();
        let tx = write(&conn).unwrap();
        tx.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','tooling','A fact','A fact')",
            [uuid::Uuid::new_v4().to_string()],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    /// The UTC date `days` ago, as backups are named.
    fn date(days: i64) -> String {
        Connection::open_in_memory()
            .unwrap()
            .query_row("SELECT date('now',?1)", [format!("-{days} days")], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn set_modified(path: &Path, ago: Duration) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - ago)
            .unwrap();
    }

    fn names(db: &Path) -> Vec<String> {
        list(db).unwrap().iter().map(Backup::name).collect()
    }

    // The first write of a day makes that day's backup; a second makes none.
    #[test]
    fn the_first_write_of_a_day_makes_one_backup_and_the_second_none() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        add(&db);
        let made = list(&db).unwrap();
        assert_eq!(names(&db), [format!("memories-{}.sqlite3", date(0))]);
        add(&db);
        let again = list(&db).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].modified, made[0].modified);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&made[0].path), 0o600);
            assert_eq!(mode(&folder(&db)), 0o700);
        }
    }

    // The newest backup's age decides, not its date: one made late yesterday
    // waits until a day has passed.
    #[test]
    fn a_backup_is_due_a_day_after_the_newest() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        add(&db);
        let yesterday = folder(&db).join(format!("memories-{}.sqlite3", date(1)));
        fs::rename(&list(&db).unwrap()[0].path, &yesterday).unwrap();
        set_modified(&yesterday, DAY - HOUR);
        add(&db);
        assert_eq!(names(&db).len(), 1);
        set_modified(&yesterday, DAY + HOUR);
        add(&db);
        assert_eq!(
            names(&db),
            [
                format!("memories-{}.sqlite3", date(0)),
                format!("memories-{}.sqlite3", date(1))
            ]
        );
    }

    // An eighth backup deletes the oldest; migrate's backups beside the store
    // and other files in the folder stay.
    #[test]
    fn an_eighth_backup_deletes_the_oldest_and_nothing_else() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        add(&db);
        let today = list(&db).unwrap().remove(0).path;
        for days in 1..=KEEP as i64 {
            let old = folder(&db).join(format!("memories-{}.sqlite3", date(days)));
            fs::copy(&today, &old).unwrap();
            set_modified(&old, DAY * days as u32);
        }
        fs::remove_file(&today).unwrap();
        let migrated = dir.path().join("memories.sqlite3.v3-20261001T000000Z.bak");
        let other = folder(&db).join("notes.txt");
        fs::write(&migrated, "").unwrap();
        fs::write(&other, "").unwrap();
        assert_eq!(names(&db).len(), KEEP);
        add(&db);
        let expected: Vec<String> = (0..KEEP as i64)
            .map(|days| format!("memories-{}.sqlite3", date(days)))
            .collect();
        assert_eq!(names(&db), expected);
        assert!(migrated.exists() && other.exists());
        let made = now(&db).unwrap();
        assert!(made.deleted.is_empty());
    }

    // A failed backup never fails the write; the failure is noted for
    // doctor, the next write does not try again at once, and the next
    // backup that succeeds clears the note.
    #[test]
    fn a_failing_backup_leaves_the_write_stored_and_notes_why() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        let blocked = folder(&db).join(format!("memories-{}.sqlite3", date(0)));
        fs::create_dir_all(&blocked).unwrap();
        add(&db);
        assert!(has_memories(&db).unwrap());
        assert!(names(&db).is_empty());
        let action = status(&db).unwrap().action().unwrap();
        assert!(
            action.starts_with("Automatic backups: the last one failed (")
                && action.contains(&blocked.display().to_string())
                && action.ends_with("); fix the cause, then run `user-memory backup now`"),
            "{action}"
        );
        assert!(files(&db).unwrap().contains(&folder(&db).join(FAILURE)));
        fs::remove_dir(&blocked).unwrap();
        let failed_ago = |ago: Duration| {
            let key = key(&db);
            let when = Instant::now().checked_sub(ago).unwrap();
            with_state(|state| state.failed.insert(key, when));
        };
        failed_ago(59 * MINUTE);
        add(&db);
        assert!(names(&db).is_empty(), "retried within the hour");
        failed_ago(61 * MINUTE);
        add(&db);
        assert_eq!(names(&db).len(), 1, "tried again after the hour");
        fs::remove_file(&list(&db).unwrap()[0].path).unwrap();
        note_failure(&db, "an earlier failure");
        now(&db).unwrap();
        assert_eq!(names(&db).len(), 1);
        assert!(!folder(&db).join(FAILURE).exists());
        assert_eq!(status(&db).unwrap().action(), None);
    }

    // A copy cut short is never listed as a backup; the next backup deletes
    // it once it is a day old, and uninstall's purge finds every one.
    #[test]
    fn copies_cut_short_are_never_backups_and_are_cleaned_up() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        add(&db);
        let folder = folder(&db);
        let old = folder.join(format!(".memories-{}.sqlite3{PARTIAL}1", date(2)));
        let fresh = folder.join(format!(".memories-{}.sqlite3{PARTIAL}2", date(0)));
        let other = folder.join(".notes.partial-3");
        for file in [&old, &fresh, &other] {
            fs::write(file, "").unwrap();
        }
        set_modified(&old, DAY + HOUR);
        assert_eq!(names(&db).len(), 1);
        let listed = files(&db).unwrap();
        assert!(listed.contains(&old) && listed.contains(&fresh) && !listed.contains(&other));
        now(&db).unwrap();
        assert!(!old.exists() && fresh.exists() && other.exists());
    }

    // A folder that cannot be read is an error, not an empty list.
    #[test]
    fn an_unreadable_folder_is_an_error() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        fs::write(folder(&db), "").unwrap();
        assert!(list(&db).is_err());
        assert!(status(&db).is_err());
        fs::remove_file(folder(&db)).unwrap();
        fs::create_dir_all(folder(&db).join(FAILURE)).unwrap();
        assert!(status(&db).is_err(), "an unreadable failure note");
    }

    // A folder that cannot be created names itself.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_created_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let db = store(&dir.path().join("store"));
        add(&db);
        fs::remove_dir_all(folder(&db)).unwrap();
        let store_dir = dir.path().join("store");
        fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let err = now(&db).err().map(|err| err.to_string());
        fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            err,
            Some(format!("Cannot create {}", folder(&db).display()))
        );
    }

    // A writer that finds this process already backing its store up leaves
    // the backup to that writer, so it never waits for the copy; the mark
    // goes when that backup ends.
    #[test]
    fn a_writer_leaves_a_backup_under_way_to_the_writer_making_it() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        let key = key(&db);
        with_state(|state| state.copying.insert(key.clone()));
        add(&db);
        assert!(names(&db).is_empty());
        with_state(|state| state.copying.remove(&key));
        add(&db);
        assert_eq!(names(&db).len(), 1);
        assert!(!with_state(|state| state.copying.contains(&key)));
    }

    // One store reached by two spellings, here through a link to its
    // folder as setup may name it, shares this process's backup state: a
    // failure through one is not tried again at once through the other.
    #[cfg(unix)]
    #[test]
    fn every_spelling_of_a_store_shares_its_backup_state() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();
        let db = store(&real);
        let alias = dir.path().join("alias").join("memories.sqlite3");
        let blocked = folder(&db).join(format!("memories-{}.sqlite3", date(0)));
        fs::create_dir_all(&blocked).unwrap();
        add(&db);
        fs::remove_dir(&blocked).unwrap();
        back_up_if_due(&alias);
        assert!(names(&db).is_empty(), "tried again within the hour");
    }

    // A store just created holds nothing worth a backup.
    #[test]
    fn a_store_without_memories_gets_no_backup() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        let conn = connect(&db).unwrap();
        let tx = write(&conn).unwrap();
        tx.execute("DELETE FROM memories", []).unwrap();
        tx.commit().unwrap();
        assert!(!folder(&db).exists());
        assert_eq!(status(&db).unwrap().action(), None);
    }

    // The copy holds what the write-ahead log holds and the store file does
    // not have yet.
    #[test]
    fn a_backup_holds_writes_not_yet_copied_from_the_log() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        // Kept open, so the log stays until the test reads it.
        let _open = connect(&db).unwrap();
        add(&db);
        let alone = dir.path().join("alone");
        fs::create_dir(&alone).unwrap();
        fs::copy(&db, alone.join("m.sqlite3")).unwrap();
        let without_log = memory_rows(&Connection::open(alone.join("m.sqlite3")).unwrap()).unwrap();
        assert_eq!(without_log, 0, "the memory is still only in the log");
        let backup = &list(&db).unwrap()[0].path;
        let copy = Connection::open_with_flags(backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(memory_rows(&copy).unwrap(), 1);
        let mode: String = copy
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        assert!(!suffixed(backup, "-wal").exists());
    }

    // Doctor's line and its Action needed step for each state.
    #[test]
    fn doctor_reports_the_newest_backup_and_asks_when_one_is_missing_or_old() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        let shown = folder(&db).display().to_string();
        let fresh = status(&db).unwrap();
        assert_eq!(
            fresh.line(),
            format!("Automatic backups: none yet in {shown}")
        );
        assert_eq!(fresh.action(), None, "nothing to back up yet");
        // A memory written by a process that made no backup, such as 0.5.
        let conn = connect(&db).unwrap();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('0bd1ba49-2b63-4aab-8d57-38d9be10a281','fact','tooling','A fact','A fact')",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            status(&db).unwrap().action().as_deref(),
            Some("Automatic backups: none yet; run `user-memory backup now`")
        );
        let backup = now(&db).unwrap().backup.path;
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        let current = status(&db).unwrap();
        assert_eq!(
            current.line(),
            format!("Automatic backups: newest {name}, 0 minutes old (1 kept in {shown})")
        );
        assert_eq!(current.action(), None);
        // Old, but nothing written since, even at the same instant: nothing
        // to ask.
        let _ = fs::remove_file(suffixed(&db, "-wal"));
        let three_days_ago = SystemTime::now() - 3 * DAY;
        for file in [&backup, &db] {
            fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(three_days_ago)
                .unwrap();
        }
        assert_eq!(status(&db).unwrap().action(), None);
        set_modified(&db, 4 * DAY);
        assert_eq!(status(&db).unwrap().action(), None);
        set_modified(&db, DAY);
        assert_eq!(
            status(&db).unwrap().action().as_deref(),
            Some(
                "Automatic backups: the newest is 3 days old though memories changed since; run `user-memory backup now`"
            )
        );
        set_modified(&backup, 47 * HOUR);
        assert_eq!(status(&db).unwrap().action(), None);
        set_modified(&backup, 49 * HOUR);
        assert!(status(&db).unwrap().action().is_some());
    }

    // In write-ahead log mode a write reaches the log first; the store file
    // changes only when the log is copied in.
    #[test]
    fn a_write_still_in_the_log_counts_as_a_change() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        // Kept open, so the log is not copied in and removed on close.
        let _open = connect(&db).unwrap();
        add(&db);
        let backup = list(&db).unwrap().remove(0).path;
        set_modified(&backup, 3 * DAY);
        set_modified(&db, 4 * DAY);
        set_modified(&suffixed(&db, "-wal"), 4 * DAY);
        assert_eq!(status(&db).unwrap().action(), None, "the log is older");
        set_modified(&suffixed(&db, "-wal"), DAY);
        assert!(status(&db).unwrap().action().is_some());
    }

    #[test]
    fn ages_read_in_whole_minutes_hours_or_days() {
        for (seconds, shown) in [
            (59, "0 minutes"),
            (60, "1 minute"),
            (3599, "59 minutes"),
            (3600, "1 hour"),
            (2 * 86400 - 1, "47 hours"),
            (2 * 86400, "2 days"),
        ] {
            assert_eq!(describe_age(Duration::from_secs(seconds)), shown);
        }
    }

    // backup list: newest first with time, memories and size, then the
    // restore step naming the newest.
    #[test]
    fn list_shows_each_backup_newest_first_and_how_to_restore() {
        let dir = tempdir().unwrap();
        let db = store(dir.path());
        let shown = folder(&db).display().to_string();
        assert_eq!(
            describe(&db).unwrap(),
            format!(
                "No automatic backups yet in {shown}. The first write of a day makes one; `user-memory backup now` makes one now."
            )
        );
        add(&db);
        let today = list(&db).unwrap().remove(0);
        let older = folder(&db).join(format!("memories-{}.sqlite3", date(1)));
        fs::write(&older, "not a database").unwrap();
        add(&db);
        let report = describe(&db).unwrap();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(
            lines[0],
            format!("Automatic backups in {shown}, newest first (the newest 7 are kept):")
        );
        let size = describe_size(today.bytes);
        assert!(
            lines[1].starts_with(&format!("  {}  made {}T", today.name(), date(0)))
                && lines[1].ends_with(&format!("Z  1 memory  {size}")),
            "{}",
            lines[1]
        );
        assert!(lines[2].ends_with("  unreadable  0.0 KB"), "{}", lines[2]);
        let store = db.display();
        if cfg!(windows) {
            assert_eq!(
                lines[3..].join("\n"),
                restore(&db, &today.path, true),
                "Windows prints the PowerShell form"
            );
        } else {
            assert_eq!(
                lines[3..],
                [
                    "To restore one, stop every client and server using the store, then run, with the backup you choose:".to_owned(),
                    format!("  rm -f '{store}-wal' '{store}-shm'"),
                    format!("  cp '{}' '{store}'", today.path.display()),
                ]
            );
        }
    }

    // On Windows the restore runs in PowerShell, which has no `rm -f` or
    // `cp` of that form: each path is a literal, and a missing log file is
    // no error. tests/windows_restore.rs runs these in both PowerShells.
    #[test]
    fn windows_restores_in_powershell() {
        let db = Path::new(r"C:\Users\o'neil\AppData\Local\user-memory\memories.sqlite3");
        let backup = Path::new(
            r"C:\Users\o'neil\AppData\Local\user-memory\backups\memories-2026-10-18.sqlite3",
        );
        assert_eq!(
            restore(db, backup, true),
            [
                "To restore one, stop every client and server using the store, then run in PowerShell, with the backup you choose:",
                r"  Get-Item -Force -LiteralPath 'C:\Users\o''neil\AppData\Local\user-memory\memories.sqlite3-wal', 'C:\Users\o''neil\AppData\Local\user-memory\memories.sqlite3-shm' -ErrorAction Ignore | Remove-Item -Force",
                r"  Copy-Item -Force -LiteralPath 'C:\Users\o''neil\AppData\Local\user-memory\backups\memories-2026-10-18.sqlite3' -Destination 'C:\Users\o''neil\AppData\Local\user-memory\memories.sqlite3'",
            ]
            .join("\n")
        );
    }

    #[test]
    fn sizes_read_in_kilobytes_below_a_megabyte() {
        for (bytes, shown) in [
            (73_700, "73.7 KB"),
            (999_999, "1000.0 KB"),
            (1_000_000, "1.0 MB"),
            (1_300_000, "1.3 MB"),
        ] {
            assert_eq!(describe_size(bytes), shown);
        }
    }

    #[test]
    fn backups_are_named_for_the_store() {
        let names = Names::of(Path::new("/d/memories.sqlite3")).unwrap();
        assert_eq!(names.name("2026-10-18"), "memories-2026-10-18.sqlite3");
        assert_eq!(
            names.date("memories-2026-10-18.sqlite3"),
            Some("2026-10-18")
        );
        for other in [
            "memories-2026-10-1.sqlite3",
            "memories-2026x10-18.sqlite3",
            "memories-2026-10-18.db",
            "other-2026-10-18.sqlite3",
            "memories.sqlite3.v3-20261001T000000Z.bak",
        ] {
            assert_eq!(names.date(other), None, "{other}");
        }
        let bare = Names::of(Path::new("/d/store")).unwrap();
        assert_eq!(bare.name("2026-10-18"), "store-2026-10-18");
        assert_eq!(bare.date("store-2026-10-18"), Some("2026-10-18"));
    }
}
