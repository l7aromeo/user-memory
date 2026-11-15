//! Delivery matching: what an agent did lately, as terms, ranked against
//! memories' passages with the store's own keyword weights, and the bars a
//! passage must pass to be pushed after an action, pushed after a failed
//! one, or hold a command back (issues #117 and #118, measured by #125).
//!
//! - The window: each action adds the terms the agent stated or did, never
//!   a tool's result, a file's contents or the text an edit writes; the
//!   last [`WINDOW_ACTIONS`] count, newer ones more.
//! - The delivery index: a read-only copy of the keyword index's rows for
//!   one scope (stems, anchors and each passage's place, no sentence of any
//!   memory), written to a local file and looked up there without opening
//!   the store.
//! - The score: what a memory's slot 0 and one passage hold of the window,
//!   each term by its field and its rarity, as search ranks a topic.
//!
//! No hook calls this yet, so the shipped binary does not declare the
//! module. The delivery benchmark's binary (`user-memory-bench`,
//! evals/delivery/entry.rs) compiles it through a `#[path]` mirror of
//! `src/memory`: use only std, rusqlite and the mirrored modules here.

use crate::memory::index::{
    passages::{self, CONTEXT_SLOT, SLOTS},
    stems::{Field, Stems, stamp, token},
    weights::{ANCHOR_WEIGHT, rarity, weight},
};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fmt, fs,
    path::Path,
};

/// Actions the window keeps: older ones fall out. #117's starting value,
/// which the delivery benchmark cannot measure: each of its tasks is
/// matched in a window of its own and runs 4 to 7 actions, so no window
/// tried (10 to 50) ever drops one, and all decide the same.
pub(crate) const WINDOW_ACTIONS: usize = 30;

/// Actions after which a term's weight in the window halves, so the action
/// that fired counts most and one from ten actions ago a quarter. The
/// delivery benchmark found 2, 5 and 10 within 0.01 of each other on
/// every measure at 1,000 memories, so the starting value stands.
pub(crate) const WINDOW_HALF_LIFE: f64 = 5.0;

// The bars. Every push and hold needs an anchor of the action that fired,
// and rank orders what passes but never makes a passage pass. A shell
// command's passages are tested on [`Matched::share`]; every other action's
// on [`Matched::mixed`]. Both are free of the store's size, so the bars hold
// from 50 to 10,000 memories. Share alone never pushes on a prompt, whose
// many words no passage covers, so other actions keep `mixed`. The delivery
// benchmark (#125) set them on synthetic stores; the figures are at 1,000
// memories, with "about" counting a passage that belongs to the action
// whether or not the task needs it then. Together they give 0.91 of
// deliveries needed then, 0.99 about the action, and 0.87 of the needs
// that share an anchor with the work found (0.88 at 10,000). A single bar on `mixed` for
// every action finds 0.44 to 0.73 of those needs where a command has 8 to
// 12 notes; these keep 0.76 to 0.88.

/// The share of what a command could score at most that a passage needs
/// to be pushed after it (#117). 0.6 keeps a busy command's notes (band 6+
/// recall 0.98) where a bar on `mixed` loses them.
pub(crate) const COMMAND_PUSH_BAR: f64 = 0.6;

/// The share a passage needs to be pushed after a command that failed
/// (#117). Equal to [`COMMAND_PUSH_BAR`], as a lower bar gains nothing.
/// Measured with the anchor rule where half the commands fail, at 1,000
/// and 10,000 memories: at 0.5, 0.4 and 0.3 a failure pushes 2 to 9 more
/// passages on the tuning seeds, every one needed then, but finds one
/// more failure need of 75 at most, and on the report seeds none (85 of
/// 168 at 0.6 and at 0.4). Where 15% fail, a lower bar changes nothing.
pub(crate) const COMMAND_FAILURE_BAR: f64 = 0.6;

/// The share a passage needs to hold a shell command back (#118): above
/// [`COMMAND_PUSH_BAR`], as a hold interrupts the agent. At 0.75 a task
/// sees 1.69 holds at 1,000 memories, 0.91 of held passages needed then;
/// at 1.0 it would see 1.14 and lose 0.09 of anchored recall.
pub(crate) const HOLD_BAR: f64 = 0.75;

/// The `mixed` score a passage needs to be pushed after any action but a
/// command (#117): a prompt, a file, a message. 0.4 pushes on a rare
/// anchor and never on common words alone.
pub(crate) const PUSH_BAR: f64 = 0.4;

/// The `mixed` score a passage needs to be pushed after any action but a
/// command that failed (#117). Equal to [`PUSH_BAR`]: the delivery
/// benchmark cannot measure it, as only commands fail in its worlds, and
/// at 0.2 to 0.4 it decides the same.
pub(crate) const FAILURE_BAR: f64 = 0.4;

/// Programs that run the command after them, which is the one a memory
/// names: `sudo cargo publish` is `cargo publish`.
const WRAPPERS: &[&str] = &["sudo", "env", "time", "nohup", "exec", "command"];

/// File extensions and the language a memory names them by, so editing
/// `parse.rs` adds "rust" to the window.
const LANGUAGES: &[(&str, &str)] = &[
    ("c", "c"),
    ("cpp", "c++"),
    ("cs", "c#"),
    ("go", "go"),
    ("h", "c"),
    ("java", "java"),
    ("js", "javascript"),
    ("json", "json"),
    ("kt", "kotlin"),
    ("md", "markdown"),
    ("py", "python"),
    ("rb", "ruby"),
    ("rs", "rust"),
    ("sh", "shell"),
    ("sql", "sql"),
    ("swift", "swift"),
    ("toml", "toml"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
];

/// What an agent did, by the kind of hook that saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The user's prompt.
    Prompt,
    /// A subagent's task, its first window.
    Task,
    /// A plan's items, one per line.
    Plan,
    /// A shell command line.
    Command,
    /// A file read or edited, by its path relative to the project root.
    File,
    /// A message the agent sent.
    Message,
    /// The agent's own memory search, by its topic.
    Search,
}

/// One action: its kind, what the agent stated or did (a prompt's words, a
/// command line, a path), and whether it failed. Nothing else of a tool
/// call joins the window: there is no field for its result, a file's
/// contents or an edit's text.
#[derive(Clone, Debug)]
pub(crate) struct Action {
    pub(crate) kind: Kind,
    pub(crate) text: String,
    pub(crate) failed: bool,
}

/// The terms one action adds to the window: stems as the keyword index
/// holds them, and anchors matched as written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::memory) struct Terms {
    pub(in crate::memory) stems: BTreeSet<String>,
    pub(in crate::memory) anchors: BTreeSet<String>,
}

impl Terms {
    fn words(&mut self, stems: &mut Stems, text: &str) {
        self.stems.extend(stems.all(text));
    }
}

/// The segments of a command line: the commands of a chain or pipe,
/// split at `&&`, `||`, `;`, `|` and line ends outside quotes, each as its
/// words without quotes.
fn segments(line: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    let end_word = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '&' | '|') => {
                // `&&`, `||`, a pipe, or a command sent to the background.
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                end_word(&mut word, &mut words);
                segments.push(std::mem::take(&mut words));
            }
            (None, ';' | '\n') => {
                end_word(&mut word, &mut words);
                segments.push(std::mem::take(&mut words));
            }
            (None, c) if c.is_whitespace() => end_word(&mut word, &mut words),
            (None, c) => word.push(c),
        }
    }
    end_word(&mut word, &mut words);
    segments.push(words);
    segments.retain(|words| !words.is_empty());
    segments
}

/// Whether `word` sets a variable for the command after it (`RUST_LOG=1`).
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// A shell command's terms: each segment's program with its subcommand,
/// as a memory's code span names them (`cargo publish`), and its path
/// arguments, as anchors and as words. The subcommand is the first word
/// argument after the program and its flags that is no path, so
/// `cargo --locked test` names `cargo test` and `git -C ../repo push`
/// names `git push`. A flag's word value is taken for the subcommand
/// (`kubectl -n prod apply` names `kubectl prod`): the delivery benchmark
/// (#125) found this rule's deliveries 0.90 needed then and 0.98 about the
/// action at 1,000 memories, against 0.85 and 0.92 for taking the first
/// word some memory names, and 0.86 and 0.94 for the word after a flag,
/// which mistake a later argument for the subcommand more often than they
/// find a flag's value.
fn command_terms(line: &str, stems: &mut Stems, terms: &mut Terms) {
    for words in segments(line) {
        let mut rest = words
            .iter()
            .map(String::as_str)
            .skip_while(|word| assignment(word) || WRAPPERS.contains(word));
        let Some(program) = rest.next() else {
            continue;
        };
        let name = program.rsplit('/').next().unwrap_or(program);
        let args: Vec<&str> = rest.collect();
        let sub = args.first().filter(|arg| !arg.starts_with('-'));
        terms.anchors.insert(name.to_lowercase());
        terms.anchors.extend(passages::anchors(program));
        terms.words(stems, program);
        if let Some(sub) = sub {
            terms.words(stems, sub);
        }
        if let Some(word) = args
            .iter()
            .find(|arg| word_argument(arg) && !arg.contains('/'))
        {
            terms
                .anchors
                .insert(format!("{name} {word}").to_lowercase());
        }
        for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
            let found = passages::anchors(arg);
            if !found.is_empty() {
                terms.words(stems, arg);
                terms.anchors.extend(found);
            }
        }
    }
}

/// Whether a command's argument is a word a subcommand could be: it starts
/// with a letter and holds no space, so no flag, path or quoted message.
fn word_argument(arg: &str) -> bool {
    arg.starts_with(|c: char| c.is_ascii_alphabetic())
        && !arg.contains(char::is_whitespace)
        && !assignment(arg)
}

/// A file's terms: its path relative to the project root, every tail of it
/// and every folder it is in as anchors, its folder and file names as
/// words, and the language its extension names.
fn file_terms(path: &str, stems: &mut Stems, terms: &mut Terms) {
    let path = path.trim().trim_start_matches("./");
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    for at in 0..parts.len() {
        terms.anchors.insert(parts[at..].join("/").to_lowercase());
        if at > 0 {
            let folder = parts[..at].join("/").to_lowercase();
            terms.anchors.insert(format!("{folder}/"));
            terms.anchors.insert(folder);
        }
    }
    terms.words(stems, path);
    if let Some((_, extension)) = parts.last().and_then(|name| name.rsplit_once('.'))
        && let Some((_, language)) = LANGUAGES
            .iter()
            .find(|(ext, _)| ext.eq_ignore_ascii_case(extension))
    {
        terms.words(stems, language);
    }
}

/// The terms `action` adds to the window.
pub(in crate::memory) fn terms(action: &Action, stems: &mut Stems) -> Terms {
    let mut terms = Terms::default();
    match action.kind {
        Kind::Command => command_terms(&action.text, stems, &mut terms),
        Kind::File => file_terms(&action.text, stems, &mut terms),
        Kind::Prompt | Kind::Task | Kind::Plan | Kind::Message | Kind::Search => {
            terms.words(stems, &action.text);
            terms.anchors.extend(passages::query_anchors(&action.text));
        }
    }
    terms
}

/// The window's settings: the constants above, which the delivery
/// benchmark varies to measure them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Tuning {
    pub(crate) window_actions: usize,
    pub(crate) half_life: f64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            window_actions: WINDOW_ACTIONS,
            half_life: WINDOW_HALF_LIFE,
        }
    }
}

/// An agent context's recent actions, newest last, with the stems of the
/// words they used.
pub(crate) struct Window {
    tuning: Tuning,
    stems: Stems,
    actions: VecDeque<(Kind, Terms, bool)>,
}

impl Default for Window {
    fn default() -> Self {
        Self::with(Tuning::default())
    }
}

impl Window {
    /// An empty window with other settings than the constants.
    pub(crate) fn with(tuning: Tuning) -> Self {
        Self {
            tuning,
            stems: Stems::new(),
            actions: VecDeque::new(),
        }
    }

    /// Adds `action`, the one that fires the next match; the oldest past
    /// [`WINDOW_ACTIONS`] falls out.
    pub(crate) fn push(&mut self, action: &Action) {
        let terms = terms(action, &mut self.stems);
        self.actions.push_back((action.kind, terms, action.failed));
        while self.actions.len() > self.tuning.window_actions {
            self.actions.pop_front();
        }
    }
}

/// How the index keys a term: a stem or an anchor, as its hex token.
fn key(anchor: bool, term: &str) -> Vec<u8> {
    let mut key = vec![if anchor { b'a' } else { b's' }];
    key.extend(token(term).into_bytes());
    key
}

/// What the window holds of one term.
#[derive(Clone, Copy, Default)]
struct Wanted {
    /// Its weight by the age of the newest action holding it.
    weight: f64,
    /// The action that fired holds it.
    fired: bool,
    /// The action that fired failed and holds it.
    failed: bool,
    /// It is an anchor of the action that fired.
    anchored: bool,
    /// It is an anchor of the command that fired.
    command: bool,
    /// The fewest memories holding an anchor of the action that fired that
    /// this match holds; 0 for none.
    fewest: usize,
}

/// One passage the window matched, with the memory's best score.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Matched {
    /// The memory's position in the index (see [`DeliveryIndex::id`]).
    pub(crate) memory: usize,
    /// The passage's slot (see `index::rows`).
    pub(crate) slot: i64,
    /// Its place in the content, or the context's for [`CONTEXT_SLOT`].
    pub(crate) start: usize,
    pub(crate) length: usize,
    pub(crate) score: f64,
    /// The match scored free of the store's size, which the bars of every
    /// action but a command test: each term's worth falls with the memories
    /// holding it, an anchor's as `1/sqrt(holders)` and a word's as
    /// `1/holders`.
    pub(crate) mixed: f64,
    /// The score as a share of what the action that fired could score at
    /// most, which a command's bars test.
    pub(crate) share: f64,
    /// The fewest memories holding an anchor of the action that fired that
    /// this match holds; 0 for none.
    pub(crate) fewest: usize,
    /// It holds a term of the action that fired.
    pub(crate) fired: bool,
    /// It holds a term of the failed action that fired.
    pub(crate) failed: bool,
    /// It holds an anchor of the action that fired.
    pub(crate) anchored: bool,
    /// It holds an anchor of the command that fired.
    pub(crate) command: bool,
    /// The action that fired is a shell command, so the command bars apply.
    pub(crate) on_command: bool,
}

impl Matched {
    /// Pushed after the action: an anchor of it, and past
    /// [`COMMAND_PUSH_BAR`] after a command or [`PUSH_BAR`] after anything
    /// else. Stems alone never push.
    pub(crate) fn push(&self) -> bool {
        self.fired && self.anchored && self.passes(COMMAND_PUSH_BAR, PUSH_BAR)
    }
    /// Pushed after a failed action: an anchor of it, and past
    /// [`COMMAND_FAILURE_BAR`] or [`FAILURE_BAR`]. Stems alone never push
    /// after a failure: they made three wrong pushes for each right one.
    pub(crate) fn failure(&self) -> bool {
        self.failed && self.anchored && self.passes(COMMAND_FAILURE_BAR, FAILURE_BAR)
    }
    /// Holds the command: one of its anchors, and past [`HOLD_BAR`].
    /// Stems alone never hold.
    pub(crate) fn hold(&self) -> bool {
        self.command && self.share >= HOLD_BAR
    }
    /// Past `command` on the share after a command, else past `other` on
    /// the mixed score.
    fn passes(&self, command: f64, other: f64) -> bool {
        if self.on_command {
            self.share >= command
        } else {
            self.mixed >= other
        }
    }
}

/// The file the delivery index is written to starts with this, its format
/// in the last byte.
const MAGIC: &[u8; 8] = b"UMDELIV1";

/// A memory id as the store writes it: a hyphenated UUID.
const ID_BYTES: usize = 36;

/// Bytes of one row, one directory entry and one posting in the file.
const ROW_BYTES: usize = 14;
const ENTRY_BYTES: usize = 20;
const POSTING_BYTES: usize = 5;

/// Why a delivery index can't be built or read.
#[derive(Debug)]
pub(crate) enum Error {
    Store(rusqlite::Error),
    File(std::io::Error),
    /// The store's keyword index is missing, stale or has memories queued.
    NotCurrent,
    /// The file isn't a delivery index this code wrote, or was cut short.
    Format,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Store(e) => write!(f, "the store: {e}"),
            Error::File(e) => write!(f, "the delivery index file: {e}"),
            Error::NotCurrent => write!(
                f,
                "the store's keyword index isn't current for this code; run `user-memory reindex`"
            ),
            Error::Format => write!(f, "the delivery index file is not one this code wrote"),
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Store(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::File(e)
    }
}

/// One row of the keyword index, as the delivery index keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    memory: u32,
    slot: u16,
    start: u32,
    length: u32,
}

/// One memory's rows for building: its slot 0 and passages, each with its
/// stems by field and its anchors.
pub(crate) struct Source {
    pub(in crate::memory) id: String,
    pub(in crate::memory) rows: Vec<SourceRow>,
}

/// One row of a memory to index (see `index::rows`).
pub(in crate::memory) struct SourceRow {
    pub(in crate::memory) slot: i64,
    pub(in crate::memory) start: usize,
    pub(in crate::memory) length: usize,
    pub(in crate::memory) stems: Vec<(String, Field)>,
    pub(in crate::memory) anchors: Vec<String>,
}

/// One term's rows while the index is built: each row with the field
/// holding the term there, and the memories holding it.
type Postings = (Vec<(u32, u8)>, BTreeSet<u32>);

/// The rows holding one term, with the field holding it in each, and how
/// many memories hold it.
struct Found {
    holders: usize,
    rows: Vec<(Row, Field)>,
}

/// The delivery index: the bytes of its file, read in place.
pub(crate) struct DeliveryIndex {
    bytes: Vec<u8>,
    memories: usize,
    ids_at: usize,
    rows: usize,
    rows_at: usize,
    entries: usize,
    entries_at: usize,
    pool_at: usize,
    postings_at: usize,
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, Error> {
    bytes
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::Format)
}

fn put(bytes: &mut Vec<u8>, value: usize) -> Result<(), Error> {
    bytes.extend(
        u32::try_from(value)
            .map_err(|_| Error::Format)?
            .to_le_bytes(),
    );
    Ok(())
}

impl DeliveryIndex {
    /// The file's bytes for `memories`: their ids, their rows, and each
    /// term's rows sorted by its key, so a lookup reads only its terms.
    pub(crate) fn write(memories: &[Source]) -> Result<Vec<u8>, Error> {
        let mut rows: Vec<Row> = Vec::new();
        // Each term's rows with the field holding it, and the memories
        // holding it.
        let mut postings: BTreeMap<Vec<u8>, Postings> = BTreeMap::new();
        for (m, memory) in memories.iter().enumerate() {
            let m = u32::try_from(m).map_err(|_| Error::Format)?;
            for row in &memory.rows {
                let r = u32::try_from(rows.len()).map_err(|_| Error::Format)?;
                rows.push(Row {
                    memory: m,
                    slot: u16::try_from(row.slot).map_err(|_| Error::Format)?,
                    start: u32::try_from(row.start).map_err(|_| Error::Format)?,
                    length: u32::try_from(row.length).map_err(|_| Error::Format)?,
                });
                let terms = row
                    .stems
                    .iter()
                    .map(|(stem, field)| (key(false, stem), *field as u8))
                    .chain(
                        row.anchors
                            .iter()
                            .map(|anchor| (key(true, anchor), Field::Body as u8)),
                    );
                for (key, field) in terms {
                    let (list, holders) = postings.entry(key).or_default();
                    list.push((r, field));
                    holders.insert(m);
                }
            }
        }
        let mut bytes = MAGIC.to_vec();
        let stamp = stamp();
        put(&mut bytes, stamp.len())?;
        bytes.extend(stamp.as_bytes());
        put(&mut bytes, memories.len())?;
        for memory in memories {
            let mut id = memory.id.clone().into_bytes();
            if id.len() != ID_BYTES {
                return Err(Error::Format);
            }
            bytes.append(&mut id);
        }
        put(&mut bytes, rows.len())?;
        for row in &rows {
            bytes.extend(row.memory.to_le_bytes());
            bytes.extend(row.slot.to_le_bytes());
            bytes.extend(row.start.to_le_bytes());
            bytes.extend(row.length.to_le_bytes());
        }
        put(&mut bytes, postings.len())?;
        let mut pool: Vec<u8> = Vec::new();
        let mut lists: Vec<u8> = Vec::new();
        for (key, (list, holders)) in &postings {
            put(&mut bytes, pool.len())?;
            put(&mut bytes, key.len())?;
            put(&mut bytes, holders.len())?;
            put(&mut bytes, lists.len() / POSTING_BYTES)?;
            put(&mut bytes, list.len())?;
            pool.extend(key);
            for (row, field) in list {
                lists.extend(row.to_le_bytes());
                lists.push(*field);
            }
        }
        put(&mut bytes, pool.len())?;
        bytes.extend(pool);
        bytes.extend(lists);
        Ok(bytes)
    }

    /// Reads the index from `bytes`, checking its sections fit, without
    /// copying them.
    pub(crate) fn read(bytes: Vec<u8>) -> Result<Self, Error> {
        if !bytes.starts_with(MAGIC) {
            return Err(Error::Format);
        }
        let mut at = MAGIC.len();
        let stamp_len = u32_at(&bytes, at)? as usize;
        at += 4;
        if bytes.get(at..at + stamp_len) != Some(stamp().as_bytes()) {
            return Err(Error::Format);
        }
        at += stamp_len;
        let memories = u32_at(&bytes, at)? as usize;
        let ids_at = at + 4;
        at = ids_at + memories * ID_BYTES;
        let rows = u32_at(&bytes, at)? as usize;
        let rows_at = at + 4;
        at = rows_at + rows * ROW_BYTES;
        let entries = u32_at(&bytes, at)? as usize;
        let entries_at = at + 4;
        at = entries_at + entries * ENTRY_BYTES;
        let pool_len = u32_at(&bytes, at)? as usize;
        let pool_at = at + 4;
        let postings_at = pool_at + pool_len;
        let index = Self {
            bytes,
            memories,
            ids_at,
            rows,
            rows_at,
            entries,
            entries_at,
            pool_at,
            postings_at,
        };
        if entries > 0 {
            let (_, _, first, count) = index.entry(entries - 1)?;
            if index.bytes.len() < postings_at + (first + count) * POSTING_BYTES {
                return Err(Error::Format);
            }
        } else if index.bytes.len() < postings_at {
            return Err(Error::Format);
        }
        Ok(index)
    }

    /// Reads the index from its file at `path`.
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        Self::read(fs::read(path)?)
    }

    /// How many memories it holds.
    pub(crate) fn len(&self) -> usize {
        self.memories
    }

    /// The id of memory `m`; a memory past the count is a damaged file.
    pub(crate) fn id(&self, m: usize) -> Result<&str, Error> {
        if m >= self.memories {
            return Err(Error::Format);
        }
        let at = self.ids_at + m * ID_BYTES;
        self.bytes
            .get(at..at + ID_BYTES)
            .and_then(|id| std::str::from_utf8(id).ok())
            .ok_or(Error::Format)
    }

    /// Row `r`, checked where it is read rather than in a pass over every
    /// row at load, which a hook's time can't afford: a row past the count,
    /// or naming a memory past it, is a damaged file.
    fn row(&self, r: usize) -> Result<Row, Error> {
        if r >= self.rows {
            return Err(Error::Format);
        }
        let at = self.rows_at + r * ROW_BYTES;
        let slot = self
            .bytes
            .get(at + 4..at + 6)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or(Error::Format)?;
        let memory = u32_at(&self.bytes, at)?;
        if memory as usize >= self.memories {
            return Err(Error::Format);
        }
        Ok(Row {
            memory,
            slot,
            start: u32_at(&self.bytes, at + 6)?,
            length: u32_at(&self.bytes, at + 10)?,
        })
    }

    /// Entry `e` of the directory: its key, how many memories hold it, and
    /// its first posting and their count.
    fn entry(&self, e: usize) -> Result<(&[u8], usize, usize, usize), Error> {
        let at = self.entries_at + e * ENTRY_BYTES;
        let key_at = self.pool_at + u32_at(&self.bytes, at)? as usize;
        let key_len = u32_at(&self.bytes, at + 4)? as usize;
        let key = self
            .bytes
            .get(key_at..key_at + key_len)
            .ok_or(Error::Format)?;
        Ok((
            key,
            u32_at(&self.bytes, at + 8)? as usize,
            u32_at(&self.bytes, at + 12)? as usize,
            u32_at(&self.bytes, at + 16)? as usize,
        ))
    }

    /// The rows holding `key`, each with the field holding it there, and
    /// how many memories hold it.
    fn postings(&self, key: &[u8]) -> Result<Option<Found>, Error> {
        let (mut low, mut high) = (0, self.entries);
        while low < high {
            let mid = (low + high) / 2;
            let (found, holders, first, count) = self.entry(mid)?;
            match found.cmp(key) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    // The postings must fit the file before room is made
                    // for them: a damaged count would ask for gigabytes.
                    let end = first
                        .checked_add(count)
                        .and_then(|n| n.checked_mul(POSTING_BYTES))
                        .and_then(|n| n.checked_add(self.postings_at));
                    if end.is_none_or(|end| end > self.bytes.len()) {
                        return Err(Error::Format);
                    }
                    let mut list = Vec::with_capacity(count);
                    for p in first..first + count {
                        let at = self.postings_at + p * POSTING_BYTES;
                        let row = self.row(u32_at(&self.bytes, at)? as usize)?;
                        let field = *self.bytes.get(at + 4).ok_or(Error::Format)?;
                        let field = *Field::ALL.get(usize::from(field)).ok_or(Error::Format)?;
                        list.push((row, field));
                    }
                    return Ok(Some(Found {
                        holders,
                        rows: list,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// The first passage row of memory `m`, which a match on its slot 0
    /// alone shows: rows are written by memory, in slot order from slot 0.
    fn first_passage(&self, m: u32) -> Result<Option<Row>, Error> {
        let (mut low, mut high) = (0, self.rows);
        while low < high {
            let mid = (low + high) / 2;
            if self.row(mid)?.memory < m {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let mut r = low;
        while r < self.rows {
            let row = self.row(r)?;
            if row.memory != m {
                break;
            }
            if row.slot != 0 {
                return Ok(Some(row));
            }
            r += 1;
        }
        Ok(None)
    }

    /// The passages `window` matches, one per memory (its best), strongest
    /// first, each scored as search scores a topic: what slot 0 holds plus
    /// the best passage's own terms, each by its field's weight, its rarity
    /// among the indexed memories and its age in the window. A memory's
    /// other passages add nothing, as in search (#124), so a long memory
    /// matching in many places does not outrank a short one matching as
    /// well in one. Rarity is divided by the rarest possible; that changes
    /// the scale only, not the order, and search's own score is untouched.
    pub(crate) fn rank(&self, window: &Window) -> Result<Vec<Matched>, Error> {
        let mut wanted: BTreeMap<Vec<u8>, Wanted> = BTreeMap::new();
        let newest = window.actions.len().saturating_sub(1);
        let on_command = window
            .actions
            .back()
            .is_some_and(|(kind, _, _)| *kind == Kind::Command);
        for (i, (kind, terms, failed)) in window.actions.iter().enumerate() {
            let age = (newest - i) as f64;
            let fired = i == newest;
            let weight = 0.5f64.powf(age / window.tuning.half_life);
            let keys = terms
                .stems
                .iter()
                .map(|stem| (key(false, stem), false))
                .chain(terms.anchors.iter().map(|anchor| (key(true, anchor), true)));
            for (key, anchor) in keys {
                let w = wanted.entry(key).or_default();
                w.weight = w.weight.max(weight);
                w.fired |= fired;
                w.failed |= fired && *failed;
                w.anchored |= fired && anchor;
                w.command |= fired && anchor && *kind == Kind::Command;
            }
        }
        let all = self.memories as f64;
        let rarest = rarity(all, 1.0);
        /// One passage's hits: its row, its gain and what it holds of the
        /// firing action.
        type Passage = (Row, Gain, Wanted);
        /// One memory's hits: slot 0's score and flags, and each passage's.
        #[derive(Default)]
        struct Hits {
            whole: Gain,
            whole_wanted: Wanted,
            passages: BTreeMap<u16, Passage>,
        }
        let mut found: HashMap<u32, Hits> = HashMap::new();
        // What the action that fired could score at most: each of its terms
        // once as text, a term no memory holds as rare as one memory's.
        let mut possible = 0.0;
        for (key, want) in &wanted {
            let postings = self.postings(key)?;
            let anchor = key.first() == Some(&b'a');
            if want.fired {
                let holders = postings.as_ref().map_or(1, |f| f.holders.max(1));
                let base = if anchor {
                    ANCHOR_WEIGHT
                } else {
                    weight(Field::Body)
                };
                possible += base * rarity(all, holders as f64) / rarest;
            }
            let Some(Found {
                holders,
                rows: list,
            }) = postings
            else {
                continue;
            };
            let worth = |field: Field| {
                let base = if anchor { ANCHOR_WEIGHT } else { weight(field) } * want.weight;
                let held = holders.max(1) as f64;
                Gain {
                    store: base * rarity(all, held) / rarest,
                    mixed: if anchor {
                        base / held.sqrt()
                    } else {
                        base / held
                    },
                }
            };
            let mut want = *want;
            if want.anchored {
                want.fewest = holders;
            }
            let want = &want;
            for (row, field) in list {
                let hits = found.entry(row.memory).or_default();
                if row.slot == 0 {
                    hits.whole.add(worth(field));
                    merge(&mut hits.whole_wanted, want);
                } else {
                    let entry = hits.passages.entry(row.slot).or_insert((
                        row,
                        Gain::default(),
                        Wanted::default(),
                    ));
                    entry.1.add(worth(field));
                    merge(&mut entry.2, want);
                }
            }
        }
        let mut matched = Vec::with_capacity(found.len());
        for (memory, hits) in found {
            let best =
                hits.passages
                    .values()
                    .fold(None::<&Passage>, |best, candidate| match best {
                        Some(b) if b.1.store >= candidate.1.store => Some(b),
                        _ => Some(candidate),
                    });
            let (row, gain, own) = match best {
                Some((row, gain, own)) => (*row, *gain, *own),
                None => match self.first_passage(memory)? {
                    Some(row) => (row, Gain::default(), Wanted::default()),
                    None => continue,
                },
            };
            let mut flags = hits.whole_wanted;
            merge(&mut flags, &own);
            let mut total = hits.whole;
            total.add(gain);
            matched.push(Matched {
                memory: memory as usize,
                slot: i64::from(row.slot),
                start: row.start as usize,
                length: row.length as usize,
                score: total.store,
                mixed: total.mixed,
                share: if possible > 0.0 {
                    total.store / possible
                } else {
                    0.0
                },
                fired: flags.fired,
                failed: flags.failed,
                anchored: flags.anchored,
                command: flags.command,
                on_command,
                fewest: flags.fewest,
            });
        }
        matched.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.memory.cmp(&b.memory)));
        Ok(matched)
    }
}

/// A match's score by the store's rarity, and free of the store's size
/// (see [`Matched::mixed`]).
#[derive(Clone, Copy, Default)]
struct Gain {
    store: f64,
    mixed: f64,
}

impl Gain {
    fn add(&mut self, other: Gain) {
        self.store += other.store;
        self.mixed += other.mixed;
    }
}

/// Adds what `other` holds of the firing action to `into`.
fn merge(into: &mut Wanted, other: &Wanted) {
    into.fired |= other.fired;
    into.failed |= other.failed;
    into.anchored |= other.anchored;
    into.command |= other.command;
    if other.fewest > 0 && (into.fewest == 0 || other.fewest < into.fewest) {
        into.fewest = other.fewest;
    }
}

/// The memories search may deliver in a scope: active ones of the global
/// scope and of `projects`, never a guess or an observation about the user
/// (inferred memories, types `behavior` and `pattern`).
const CANDIDATES: &str = "SELECT k.key,m.id FROM search_keys k JOIN memories m ON m.id=k.id \
     WHERE m.status IN ('confirmed','tentative') AND m.source!='inferred' \
     AND m.memory_type NOT IN ('behavior','pattern') \
     AND (m.scope='global' OR (m.scope='project' AND m.project IN (SELECT value FROM json_each(?1)))) \
     ORDER BY k.key";

/// The rows of the store's keyword index at `db` for the memories search
/// may deliver in the global scope and `projects`, read without writing.
pub(crate) fn sources(db: &Path, projects: &[String]) -> Result<Vec<Source>, Error> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let current: Option<String> = conn
        .query_row("SELECT stemmed_by FROM search_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .ok();
    let queued: i64 = conn.query_row("SELECT count(*) FROM search_pending", [], |r| r.get(0))?;
    if current.as_deref() != Some(stamp().as_str()) || queued > 0 {
        return Err(Error::NotCurrent);
    }
    let scope = format!(
        "[{}]",
        projects
            .iter()
            .map(|p| format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut keys: BTreeMap<i64, usize> = BTreeMap::new();
    let mut sources = Vec::new();
    let mut statement = conn.prepare(CANDIDATES)?;
    let mut found = statement.query(params_from_iter([scope]))?;
    while let Some(r) = found.next()? {
        keys.insert(r.get(0)?, sources.len());
        sources.push(Source {
            id: r.get(1)?,
            rows: Vec::new(),
        });
    }
    let mut statement = conn.prepare(
        "SELECT rowid,category,title,type,body,anchors,start,length FROM search_words ORDER BY rowid",
    )?;
    let mut rows = statement.query([])?;
    while let Some(r) = rows.next()? {
        let number: i64 = r.get(0)?;
        let Some(&m) = keys.get(&number.div_euclid(SLOTS)) else {
            continue;
        };
        let mut stems = Vec::new();
        for (c, field) in Field::ALL.into_iter().enumerate() {
            let column: String = r.get(c + 1)?;
            stems.extend(column.split_whitespace().map(|t| (decode(t), field)));
        }
        let anchors: String = r.get(5)?;
        let start: Option<i64> = r.get(6)?;
        let length: Option<i64> = r.get(7)?;
        sources[m].rows.push(SourceRow {
            slot: number.rem_euclid(SLOTS),
            start: usize::try_from(start.unwrap_or(0)).unwrap_or(0),
            length: usize::try_from(length.unwrap_or(0)).unwrap_or(0),
            stems,
            anchors: anchors.split_whitespace().map(decode).collect(),
        });
    }
    Ok(sources)
}

/// The term a hex token stands for (see `stems::token`); a token that isn't
/// one stays as it is, and matches nothing a window holds.
fn decode(token: &str) -> String {
    let bytes: Option<Vec<u8>> = (0..token.len())
        .step_by(2)
        .map(|at| {
            token
                .get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect();
    bytes
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| token.to_owned())
}

/// The text of `matched` from the store at `db`: the passage cut from the
/// memory's content, or its context.
pub(crate) fn text(conn: &Connection, id: &str, matched: &Matched) -> Result<String, Error> {
    let (content, context): (String, String) = conn.query_row(
        "SELECT content,context FROM memories WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let source = if matched.slot == CONTEXT_SLOT {
        context
    } else {
        content
    };
    Ok(source
        .get(matched.start..matched.start + matched.length)
        .unwrap_or("")
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests fail loudly on a broken setup")]
    use super::*;
    use crate::memory::index::passages::split;

    fn action(kind: Kind, text: &str) -> Action {
        Action {
            kind,
            text: text.to_owned(),
            failed: false,
        }
    }

    fn of(kind: Kind, text: &str) -> Terms {
        terms(&action(kind, text), &mut Stems::new())
    }

    fn stems(text: &str) -> BTreeSet<String> {
        Stems::new().all(text)
    }

    #[test]
    fn a_command_adds_each_program_with_its_subcommand_and_its_paths() {
        let terms = of(
            Kind::Command,
            "RUST_LOG=1 cargo publish --dry-run --registry internal && git -C ../repo status | sudo ./scripts/release.sh 'docs/notes.md'",
        );
        for anchor in [
            "cargo",
            "cargo publish",
            "git",
            "git status",
            "release.sh",
            "./scripts/release.sh",
            "docs/notes.md",
        ] {
            assert!(terms.anchors.contains(anchor), "{anchor}: {terms:?}");
        }
        // The subcommand is the first word after the program and its flags,
        // never a path: a later word names nothing. Neither the variable nor
        // `sudo` is the program.
        for anchor in ["cargo internal", "git ../repo", "git repo"] {
            assert!(!terms.anchors.contains(anchor), "{anchor}: {terms:?}");
        }
        assert!(
            !terms
                .anchors
                .iter()
                .any(|a| a.starts_with("git -") || a.starts_with("cargo -")),
            "{terms:?}"
        );
        assert!(
            !terms
                .anchors
                .iter()
                .any(|a| a.starts_with("rust_log") || a.starts_with("sudo")),
            "{terms:?}"
        );
        // Words of the programs, subcommands and paths, never of flags or
        // other arguments: a word paired with the program is an anchor only.
        assert!(
            terms
                .stems
                .is_superset(&stems("cargo publish release scripts docs notes repo"))
        );
        for word in ["dri", "registri", "intern", "status", "rust_log", "sudo"] {
            assert!(!terms.stems.contains(word), "{word}: {terms:?}");
        }
    }

    #[test]
    fn a_subcommand_after_a_flag_or_behind_a_wrapper_is_still_named() {
        let terms = of(
            Kind::Command,
            "cargo --locked test && cargo run --bin test && git -C . commit -m fix",
        );
        for anchor in ["cargo test", "cargo run", "git commit"] {
            assert!(terms.anchors.contains(anchor), "{anchor}: {terms:?}");
        }
        for anchor in ["cargo bin", "git fix"] {
            assert!(!terms.anchors.contains(anchor), "{anchor}: {terms:?}");
        }
        // The rule's known limit: a flag's word value reads as the
        // subcommand, so `kubectl -n prod apply` names `kubectl prod`.
        let flagged = of(Kind::Command, "kubectl -n prod apply -f deploy.yml");
        assert!(flagged.anchors.contains("kubectl prod"), "{flagged:?}");
        assert!(!flagged.anchors.contains("kubectl apply"), "{flagged:?}");
        // And a wrapper no table names hides the command it runs.
        let wrapped = of(Kind::Command, "proxychains git push");
        assert!(wrapped.anchors.contains("proxychains git"), "{wrapped:?}");
        assert!(!wrapped.anchors.contains("git push"), "{wrapped:?}");
    }

    #[test]
    fn a_file_adds_its_path_its_tails_its_folders_and_its_language() {
        let terms = of(Kind::File, "./src/memory/read.rs");
        for anchor in [
            "src/memory/read.rs",
            "memory/read.rs",
            "read.rs",
            "src",
            "src/",
            "src/memory",
            "src/memory/",
        ] {
            assert!(terms.anchors.contains(anchor), "{anchor}: {terms:?}");
        }
        assert!(terms.stems.is_superset(&stems("src memory read rs rust")));
    }

    #[test]
    fn prose_adds_its_words_and_the_anchors_a_topic_would() {
        for kind in [
            Kind::Prompt,
            Kind::Task,
            Kind::Plan,
            Kind::Message,
            Kind::Search,
        ] {
            let terms = of(kind, "Release the parser with `cargo publish`");
            assert_eq!(terms.stems, stems("Release the parser with cargo publish"));
            assert!(terms.anchors.contains("cargo publish"), "{kind:?}");
            assert!(terms.anchors.contains("parser"), "{kind:?}");
        }
    }

    /// An index of `memories`, each given as its title and content, from
    /// rows built as the store builds them.
    fn index(memories: &[(&str, &str)]) -> DeliveryIndex {
        let mut stems = Stems::new();
        let sources: Vec<Source> = memories
            .iter()
            .enumerate()
            .map(|(i, (title, content))| {
                let whole: Vec<(String, Field)> = stems
                    .all(title)
                    .into_iter()
                    .map(|s| (s, Field::Title))
                    .collect();
                let held: BTreeSet<String> = whole.iter().map(|(s, _)| s.clone()).collect();
                let mut rows = vec![SourceRow {
                    slot: 0,
                    start: 0,
                    length: 0,
                    stems: whole,
                    anchors: passages::anchors(title).into_iter().collect(),
                }];
                for (slot, place) in (1..).zip(split(content)) {
                    let part = &content[place.clone()];
                    rows.push(SourceRow {
                        slot,
                        start: place.start,
                        length: place.len(),
                        stems: stems
                            .all(part)
                            .into_iter()
                            .filter(|s| !held.contains(s))
                            .map(|s| (s, Field::Body))
                            .collect(),
                        anchors: passages::anchors(part).into_iter().collect(),
                    });
                }
                Source {
                    id: format!("00000000-0000-4000-8000-{i:012}"),
                    rows,
                }
            })
            .collect();
        DeliveryIndex::read(DeliveryIndex::write(&sources).unwrap()).unwrap()
    }

    /// A store of `n` filler memories, each sharing `common`, plus `extra`.
    fn store(common: &str, extra: &[(&str, &str)]) -> DeliveryIndex {
        let filler: Vec<(String, String)> = (0..40)
            .map(|i| {
                (
                    format!("Note {i} about the {common} step"),
                    format!("Keep the {common} step tidy, case {i}."),
                )
            })
            .collect();
        let mut all: Vec<(&str, &str)> = filler
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        all.extend_from_slice(extra);
        index(&all)
    }

    fn rank(index: &DeliveryIndex, actions: &[Action]) -> Vec<Matched> {
        let mut window = Window::default();
        for action in actions {
            window.push(action);
        }
        index.rank(&window).unwrap()
    }

    fn by_id<'a>(
        index: &DeliveryIndex,
        found: &'a [Matched],
        id_end: usize,
    ) -> Option<&'a Matched> {
        let id = format!("00000000-0000-4000-8000-{id_end:012}");
        found.iter().find(|m| index.id(m.memory).unwrap() == id)
    }

    #[test]
    fn a_term_is_held_by_memories_not_passages() {
        // One long memory names the command in each of six passages, a
        // short one names it once: two holders, not seven.
        let padding =
            ["padding words keep each paragraph long enough to stand as its own passage here"; 3]
                .join(" ");
        let long: String = (0..6)
            .map(|i| format!("Step {i}: run `cargo publish` only after the tag. {padding}\n\n"))
            .collect();
        let index = store(
            "build",
            &[
                ("Publishing in steps", long.as_str()),
                ("Publish note", "Run `cargo publish` from a clean tree."),
            ],
        );
        let Some(Found { holders, rows }) = index.postings(&key(true, "cargo publish")).unwrap()
        else {
            panic!("the anchor is indexed");
        };
        assert!(rows.len() > 2, "{} rows", rows.len());
        assert_eq!(holders, 2);
        let found = rank(&index, &[action(Kind::Command, "cargo publish")]);
        assert!(
            found.iter().filter(|m| m.fewest == 2).count() == 2,
            "{found:?}"
        );
    }

    #[test]
    fn the_file_reads_back_and_a_cut_or_foreign_file_is_refused() {
        let built = DeliveryIndex::write(&[]).unwrap();
        assert_eq!(DeliveryIndex::read(built.clone()).unwrap().len(), 0);
        let index = store("deploy", &[]);
        assert_eq!(index.len(), 40);
        let mut bytes = index.bytes.clone();
        bytes.truncate(bytes.len() - 1);
        assert!(matches!(DeliveryIndex::read(bytes), Err(Error::Format)));
        let mut foreign = index.bytes.clone();
        foreign[0] = b'X';
        assert!(matches!(DeliveryIndex::read(foreign), Err(Error::Format)));
    }

    #[test]
    fn a_row_naming_a_memory_past_the_count_is_refused_where_read() {
        let index = store("deploy", &[]);
        let mut bytes = index.bytes.clone();
        // Every row names memory 40 of 40: past the last.
        for r in 0..index.rows {
            let at = index.rows_at + r * ROW_BYTES;
            bytes[at..at + 4].copy_from_slice(&40u32.to_le_bytes());
        }
        let damaged = DeliveryIndex::read(bytes).unwrap();
        let mut window = Window::default();
        window.push(&action(Kind::Prompt, "deploy step"));
        assert!(matches!(damaged.rank(&window), Err(Error::Format)));
        assert!(matches!(damaged.id(40), Err(Error::Format)));
    }

    #[test]
    fn postings_running_past_the_file_are_refused_before_room_is_made() {
        let index = store("deploy", &[]);
        assert!(index.entries > 1);
        let key = index.entry(0).unwrap().0.to_vec();
        let mut bytes = index.bytes.clone();
        // The first entry's count, which load does not check, runs far
        // past the end.
        let at = index.entries_at + 16;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let damaged = DeliveryIndex::read(bytes).unwrap();
        assert!(matches!(damaged.postings(&key), Err(Error::Format)));
    }

    #[test]
    fn nothing_passes_on_a_common_term_alone() {
        let index = store(
            "deploy",
            &[("Deploy checklist", "Check the deploy step twice.")],
        );
        let found = rank(&index, &[action(Kind::Prompt, "deploy")]);
        assert!(!found.is_empty());
        assert!(found.iter().all(|m| !m.push() && !m.hold()), "{found:?}");
    }

    #[test]
    fn a_rare_anchor_in_a_command_passes_and_a_near_miss_does_not() {
        let rule = (
            "Dry run before publishing",
            "Run `cargo publish --dry-run` before `cargo publish`; a failed upload burns the version number.",
        );
        let index = store("deploy", &[rule]);
        let found = rank(&index, &[action(Kind::Command, "cargo publish")]);
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(hit.push() && hit.hold(), "{hit:?}");
        // Another subcommand of the same program shares only its name.
        let found = rank(&index, &[action(Kind::Command, "cargo build")]);
        assert!(
            by_id(&index, &found, 40).is_none_or(|m| !m.push() && !m.hold()),
            "{found:?}"
        );
    }

    #[test]
    fn stems_alone_never_push_or_hold_even_after_a_failure() {
        let index = store(
            "deploy",
            &[(
                "Publishing crates",
                "Publishing a crate burns its version number, so publishing waits for review.",
            )],
        );
        let mut publish = action(Kind::Command, "cargo publish");
        publish.failed = true;
        let found = rank(&index, &[publish]);
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(hit.fired && hit.failed && !hit.anchored, "{hit:?}");
        assert!(!hit.push() && !hit.failure() && !hit.hold(), "{hit:?}");
    }

    #[test]
    fn older_window_terms_alone_deliver_nothing_on_an_unrelated_action() {
        let index = store(
            "deploy",
            &[(
                "Dry run before publishing",
                "Run `cargo publish --dry-run` first.",
            )],
        );
        let found = rank(
            &index,
            &[
                action(Kind::Command, "cargo publish"),
                action(Kind::File, "README.md"),
            ],
        );
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(!hit.fired && !hit.push(), "{hit:?}");
    }

    #[test]
    fn a_failed_command_pushes_its_anchored_passage_as_a_failure() {
        let index = store(
            "deploy",
            &[(
                "Signing tags",
                "Sign release tags; `git tag -s` needs the key loaded.",
            )],
        );
        let mut failed = action(Kind::Command, "git tag v1.2.0");
        let found = rank(&index, std::slice::from_ref(&failed));
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(!hit.failure(), "{hit:?}");
        failed.failed = true;
        let found = rank(&index, &[failed]);
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(hit.failed && hit.share >= COMMAND_FAILURE_BAR, "{hit:?}");
        assert!(hit.failure(), "{hit:?}");
    }

    #[test]
    fn other_actions_are_tested_on_the_mixed_score() {
        let index = store(
            "deploy",
            &[(
                "Dry run before publishing",
                "Run `cargo publish --dry-run` before `cargo publish`; a failed upload burns the version number.",
            )],
        );
        // A prompt naming the command in a code span: many words, so its
        // share is low, but the rare anchor passes on `mixed`.
        let found = rank(
            &index,
            &[action(
                Kind::Prompt,
                "Get the release out today and run `cargo publish` once the notes are written",
            )],
        );
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(!hit.on_command && hit.share < COMMAND_PUSH_BAR, "{hit:?}");
        assert!(
            hit.mixed >= PUSH_BAR && hit.push() && !hit.hold(),
            "{hit:?}"
        );
    }

    #[test]
    fn rare_words_without_an_anchor_are_never_pushed() {
        let index = store(
            "deploy",
            &[(
                "Quokka cabinet",
                "The quokka cabinet squeaks whenever the zeppelin lands.",
            )],
        );
        // Words rare enough to pass the bar on their own, and a failed
        // action naming them: still no push, as no anchor matched.
        let mut words = action(Kind::Message, "quokka cabinet zeppelin squeaks");
        words.failed = true;
        let found = rank(&index, &[words]);
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(
            hit.mixed >= PUSH_BAR && hit.failed && !hit.anchored,
            "{hit:?}"
        );
        assert!(!hit.push() && !hit.failure(), "{hit:?}");
    }

    #[test]
    fn the_best_passage_is_returned_with_its_place() {
        let content = [
            "Point one is about aardvarks, and only aardvarks: where they sleep, what they eat and how far they walk on a cold night out on the plain near the river bend, and why the young ones follow the old ones from the burrow to the termite mounds and back again before the sun comes up over the hills.",
            "Point two is about the zeppelin hangar and its glacier door, which sticks in winter and needs two people and a lever to open before the morning flight leaves, and which the crew oils every Sunday so the hinge does not freeze solid while the airship is out over the northern sea for the week.",
            "Point three is about bassoons and their reeds in the band, which crack in dry air unless they rest in a case with a damp sponge for at least an hour each day, and which the players soak in warm water before every rehearsal so the tone stays even through the long slow movements of the piece.",
        ]
        .join("\n");
        let index = store("deploy", &[("Three points", &content)]);
        let found = rank(&index, &[action(Kind::Prompt, "zeppelin glacier")]);
        let hit = by_id(&index, &found, 40).unwrap();
        let passage = &content[hit.start..hit.start + hit.length];
        assert!(passage.contains("zeppelin"), "{passage:?}");
        assert!(!passage.contains("aardvark"), "{passage:?}");
    }
}
