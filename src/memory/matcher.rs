//! Delivery matching: what an agent did lately, as terms, ranked against
//! memories' passages with the store's own keyword weights, and the bars a
//! passage must pass to be pushed after an action, pushed after a failed
//! one, or hold a command back (issues #117 and #118, measured by #125).
//!
//! - The window: each action adds the terms the agent stated or did, never
//!   a tool's result, a file's contents or the text an edit writes; the
//!   last [`WINDOW_ACTIONS`] count, newer ones more.
//! - The score: what a memory's slot 0 and one passage hold of the window,
//!   each term by its field and its rarity, as search ranks a topic.
//!
//! It reads the delivery index (`super::delivery`), a base with its overlay
//! ([`View`]). The delivery
//! benchmark's binary (`user-memory-bench`, evals/delivery/entry.rs)
//! compiles it through a `#[path]` mirror of `src/memory`: use only std,
//! rusqlite and the mirrored modules here.

use super::delivery::{DeliveryIndex, Error, POSTING_BYTES, Row, View, prefix, u32_at};
use crate::memory::index::{
    passages::{self, CONTEXT_SLOT},
    stems::{Field, Stems, token},
    weights::{ANCHOR_WEIGHT, rarity, weight},
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Range;

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
/// recall 0.98) where a bar on `mixed` loses them. It holds for a line of
/// several parts, as each part is tested as if it ran alone: with half
/// the commands chained and half the reads beside other files, it finds
/// 1.00 of the chained needs and 0.91 of the read ones at 1,000 memories,
/// and the single commands' figures stay as they were.
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

/// The share a passage needs from the terms of the action that fired
/// alone before the window's older actions add to it (#299). Below it, the
/// passage's share is its own: older steps lift a passage the step points
/// at toward a push or a hold, and no longer make one up for a step that
/// holds only a common anchor of it, such as `git`. On the delivery
/// benchmark's tuning seeds every floor from 0.2 drops all 24 holds its
/// carried tasks made that way, with needed holds unchanged; 0.2 to 0.3
/// cost at most one right delivery per store size, 0.5 up to 8. 0.25 sits
/// above the 0.21 such a step held alone in #299's case.
pub(crate) const WINDOW_SHARE_FLOOR: f64 = 0.25;

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

/// Programs that read each path they are given on its own, so a line that
/// reads several files is a part per file, each a read of its file as the
/// read tool's is: a note on one of them is as much about the line as
/// about a lone read of it. A `sed` script that looks like a path is a
/// part no memory holds, which pushes nothing. PowerShell's readers and
/// their aliases (`Get-Content`, `gc`, `type`, `Select-String`, `sls`,
/// `Get-ChildItem`, `gci`, `dir`) and cmd's `findstr` read as theirs do.
const READERS: &[&str] = &[
    "bat",
    "cat",
    "dir",
    "file",
    "findstr",
    "gc",
    "gci",
    "get-childitem",
    "get-content",
    "grep",
    "head",
    "less",
    "ls",
    "more",
    "nl",
    "rg",
    "sed",
    "select-string",
    "sls",
    "stat",
    "tail",
    "type",
    "wc",
];

/// Programs that write each file they are given, as the write tool does:
/// `tee notes/a.txt` writes `notes/a.txt`, so a line that writes several
/// files is a part per file, each a step on its file, as a redirection's
/// is.
const WRITERS: &[&str] = &["tee"];

/// Shells whose `-c` script is the command line they run, and whose
/// here-document is when they get no script otherwise ([`reads_script`]).
const SHELLS: &[&str] = &["bash", "sh", "zsh"];

/// Programs that run the script file they are given, which is what a
/// memory names: `bash tools/release.sh` runs `tools/release.sh`, and
/// `python3 tools/build.py` runs `tools/build.py` (#220). Their own terms
/// (`bash`, `python3`) would otherwise lower a note on the script below
/// the command bars, so the script is matched as if it ran alone too.
/// Each comes with its flags whose next word is code or a module, not a
/// script file (`python3 -m pytest`, `node -p 'expr'`); they differ by
/// interpreter, as a shell's `-e` takes no value and perl's `-p` neither.
/// A shell's `-c` is [`shell_script`]'s.
const INTERPRETERS: &[Interpreter] = &[
    Interpreter::new("bash", &["-c"]),
    Interpreter::new("dash", &["-c"]),
    Interpreter::new("fish", &["-c", "--command"]),
    Interpreter::new("ksh", &["-c"]),
    Interpreter::new("node", &["-e", "--eval", "-p", "--print"]),
    Interpreter::new("perl", &["-e", "-E"]),
    Interpreter::new("php", &["-r"]),
    // PowerShell reads its parameters in any case.
    Interpreter {
        name: "pwsh",
        code: &["-c", "-command", "-e", "-ec", "-encodedcommand"],
        any_case: true,
    },
    Interpreter::new("python", &["-c", "-m"]),
    Interpreter::new("python3", &["-c", "-m"]),
    Interpreter::new("ruby", &["-e"]),
    Interpreter::new("sh", &["-c"]),
    Interpreter::new("zsh", &["-c"]),
];

/// One of [`INTERPRETERS`]: its name, the flags whose next word is code,
/// and whether those flags are read in any case.
struct Interpreter {
    name: &'static str,
    code: &'static [&'static str],
    any_case: bool,
}

impl Interpreter {
    const fn new(name: &'static str, code: &'static [&'static str]) -> Self {
        Self {
            name,
            code,
            any_case: false,
        }
    }

    /// Whether `flag` takes code or a module as its next word.
    fn takes_code(&self, flag: &str) -> bool {
        self.code.iter().any(|code| {
            if self.any_case {
                code.eq_ignore_ascii_case(flag)
            } else {
                *code == flag
            }
        })
    }
}

/// The most parts of one command line scored apart, one bit of a `u64`
/// each; parts past the last are scored together as the last, which can
/// only lower their share.
const MAX_PARTS: usize = u64::BITS as usize;

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
    /// A file read or edited, by its path relative to the project root;
    /// one action of a patch names each of its files on a line.
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
pub(crate) struct Terms {
    pub(crate) stems: BTreeSet<String>,
    pub(crate) anchors: BTreeSet<String>,
    /// A command line's parts, each scored as a step of its own when the
    /// line fires: each command of a chain, pipe or subshell, and each
    /// file a reading command reads. `stems` and `anchors` are their
    /// union. Empty for every other action and for one read back from a
    /// hook's state, which only the action that fires needs.
    pub(crate) parts: Vec<Terms>,
    /// Each path a command names, as the anchors that spell it: as written,
    /// its tails and its folders. A path is one term of what the command
    /// could score at most, at its best spelling some memory holds, or as
    /// rare as one memory's when none does: a spelling no memory uses says
    /// nothing of the command. Kept, as `parts`, for the action that fires.
    pub(crate) paths: Vec<BTreeSet<String>>,
    /// The part is one file a command reads or writes, tested as a step of
    /// the file tools is (see [`Matched::on_command`]).
    pub(crate) file: bool,
}

impl Terms {
    fn words(&mut self, stems: &mut Stems, text: &str) {
        self.stems.extend(stems.all(text));
    }

    /// Adds `part` as one of a command line's parts, its terms to the line's.
    fn add_part(&mut self, part: Terms) {
        self.stems.extend(part.stems.iter().cloned());
        self.anchors.extend(part.anchors.iter().cloned());
        self.paths.extend(part.paths.iter().cloned());
        self.parts.push(part);
    }
}

/// The segments of a command line: the commands of a chain, pipe or
/// subshell, split at `&&`, `||`, `;`, `|`, `&`, parentheses, backticks
/// and line ends outside quotes, each as its words without quotes, and
/// apart from them the files its redirections name: the file `<` reads,
/// and the one `>`, `>>`, `>|`, `&>` or `<>` writes. A
/// here-document's body is text the command reads, like quoted text, so
/// it adds no segment, unless a shell reads it as its script
/// ([`reads_script`]): then its commands are segments too.
fn segments(line: &str) -> Vec<Segment> {
    lex(line)
        .into_iter()
        .map(|lexed| {
            let mut segment = Segment::default();
            for word in lexed {
                if word.redirected {
                    segment.files.push(word.text);
                } else {
                    segment.words.push(word.text);
                }
            }
            segment
        })
        .collect()
}

/// One command of a line, as [`segments`] gives it: its words, and each
/// file a redirection of it reads or writes.
#[derive(Default)]
struct Segment {
    words: Vec<String>,
    files: Vec<String>,
}

/// Where each word of a command line is in it, split as [`segments`]
/// splits the line: from the first character the word keeps to the end of
/// its last, so a word's start is past any quote that opens it.
pub(crate) fn word_spans(line: &str) -> impl Iterator<Item = Range<usize>> {
    lex(line).into_iter().flatten().map(|word| word.span)
}

/// A word of a command line, as [`segments`] gives it, where it is, and
/// whether it is the file a redirection names.
struct Word {
    text: String,
    span: Range<usize>,
    redirected: bool,
}

impl Word {
    fn new() -> Self {
        Self {
            text: String::new(),
            span: 0..0,
            redirected: false,
        }
    }
}

/// A here-document a command line opens (`<<EOF`, `<<'EOF'`, `<<-EOF`):
/// the segment whose input it is, by its place among the segments
/// [`lex_line`] gives, and where its body is in the line.
struct HereDocument {
    segment: usize,
    body: Range<usize>,
}

/// The words of a command line in their segments, for [`segments`] and
/// [`word_spans`], with the commands of a here-document a shell runs
/// after the segment it feeds.
fn lex(line: &str) -> Vec<Vec<Word>> {
    let (segments, documents) = lex_line(line);
    let mut lexed = Vec::with_capacity(segments.len());
    for (at, words) in segments.into_iter().enumerate() {
        // A command reads the last here-document it is given.
        let script = documents
            .iter()
            .rev()
            .find(|document| document.segment == at)
            .filter(|_| reads_script(&words))
            .map(|document| document.body.clone());
        lexed.push(words);
        if let Some(body) = script {
            for mut words in lex(&line[body.clone()]) {
                for word in &mut words {
                    word.span = word.span.start + body.start..word.span.end + body.start;
                }
                lexed.push(words);
            }
        }
    }
    lexed.retain(|words| !words.is_empty());
    lexed
}

/// The words of a command line in their segments, empty ones kept so a
/// [`HereDocument`] can name its segment, and the here-documents the line
/// opens. A body starts at the end of the line that opens it and runs to
/// the line that is its delimiter alone, or to the end of the command
/// when none is, as a shell reads it. `<<` in quotes or in arithmetic
/// (`$((1 << 2))`) opens none, nor does a here-string (`<<<`), whose word
/// is read as any word. A redirection outside quotes and a `[[ … ]]`
/// test marks the word after it as the file it names, and its
/// number (`2>`) is no word.
fn lex_line(line: &str) -> (Vec<Vec<Word>>, Vec<HereDocument>) {
    let mut segments = Vec::new();
    let mut documents = Vec::new();
    // Here-documents opened since the last line end: each one's segment,
    // its delimiter, and whether its lines drop their leading tabs (`<<-`).
    let mut opened: Vec<(usize, String, bool)> = Vec::new();
    // How many parentheses deep in arithmetic (`((…))`) the line is.
    let mut arithmetic = 0usize;
    // Where the line goes on past the bodies just read.
    let mut resume = 0;
    let mut words: Vec<Word> = Vec::new();
    let mut word = Word::new();
    let mut quote: Option<char> = None;
    // Whether the next word is the file of a redirection.
    let mut redirect = false;
    let mut chars = line.char_indices().peekable();
    let end_word = |word: &mut Word, words: &mut Vec<Word>, redirect: &mut bool| {
        if !word.text.is_empty() {
            word.redirected = std::mem::take(redirect);
            words.push(std::mem::replace(word, Word::new()));
        }
    };
    let push = |word: &mut Word, at: usize, c: char| {
        if word.text.is_empty() {
            word.span.start = at;
        }
        word.text.push(c);
        word.span.end = at + c.len_utf8();
    };
    while let Some((at, c)) = chars.next() {
        if at < resume {
            continue;
        }
        let next = chars.peek().map(|&(_, next)| next);
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => push(&mut word, at, c),
            (None, '\'' | '"') => quote = Some(c),
            // `&>` sends both outputs to its file: the `&` is no word.
            (None, '&') if next == Some('>') && word.text.is_empty() => {
                push(&mut word, at, c);
            }
            (None, '<') if next == Some('<') && arithmetic == 0 => {
                chars.next();
                if chars.next_if(|&(_, c)| c == '<').is_some() {
                    for (i, c) in "<<<".char_indices() {
                        push(&mut word, at + i, c);
                    }
                    continue;
                }
                end_word(&mut word, &mut words, &mut redirect);
                let tabs = chars.next_if(|&(_, c)| c == '-').is_some();
                while chars.next_if(|&(_, c)| c == ' ' || c == '\t').is_some() {}
                let delimiter = here_delimiter(&mut chars);
                if !delimiter.is_empty() {
                    opened.push((segments.len(), delimiter, tabs));
                }
            }
            (None, '>' | '<') if !(c == '<' && next == Some('<')) && !in_test(&words) => {
                // A descriptor's number or `&` before it is no word; any
                // other word before it is one (`echo a>b`).
                if word.text == "&" || word.text.chars().all(|c| c.is_ascii_digit()) {
                    word = Word::new();
                } else {
                    end_word(&mut word, &mut words, &mut redirect);
                }
                // `>>`, `>|` and `<>` are one operator.
                chars.next_if(|&(_, n)| n == '>' || (c == '>' && n == '|'));
                // `>&2` and `2>&1` copy a descriptor: the word after the `&`
                // is a number, which names no file, and `>&file` names one.
                chars.next_if(|&(_, n)| n == '&');
                redirect = true;
            }
            (None, '(' | ')' | '`') => {
                if c == '(' && (arithmetic > 0 || next == Some('(')) {
                    arithmetic += 1;
                } else if c == ')' {
                    arithmetic = arithmetic.saturating_sub(1);
                }
                // `$(` opens a command substitution: the `$` is no word.
                if word.text.ends_with('$') {
                    word.text.pop();
                    word.span.end -= '$'.len_utf8();
                }
                end_word(&mut word, &mut words, &mut redirect);
                redirect = false;
                segments.push(std::mem::take(&mut words));
            }
            (None, '&' | '|') => {
                // `&&`, `||`, a pipe, or a command sent to the background.
                if next == Some(c) {
                    chars.next();
                }
                end_word(&mut word, &mut words, &mut redirect);
                redirect = false;
                segments.push(std::mem::take(&mut words));
            }
            (None, ';' | '\n') => {
                end_word(&mut word, &mut words, &mut redirect);
                redirect = false;
                segments.push(std::mem::take(&mut words));
                if c == '\n' {
                    resume = at + c.len_utf8();
                    for (segment, delimiter, tabs) in opened.drain(..) {
                        let (body, after) = here_body(line, resume, &delimiter, tabs);
                        documents.push(HereDocument { segment, body });
                        resume = after;
                    }
                }
            }
            (None, c) if c.is_whitespace() => end_word(&mut word, &mut words, &mut redirect),
            (None, c) => push(&mut word, at, c),
        }
    }
    end_word(&mut word, &mut words, &mut redirect);
    segments.push(words);
    (segments, documents)
}

/// Whether a segment's words so far open a `[[ … ]]` test, where `<` and
/// `>` compare strings.
fn in_test(words: &[Word]) -> bool {
    words.first().is_some_and(|word| word.text == "[[")
        && !words.iter().any(|word| word.text == "]]")
}

/// The delimiter word after `<<`, without its quotes and backslashes
/// (`'EOF'`, `"EOF"` and `\EOF` are `EOF`), up to a space or an operator
/// outside quotes.
fn here_delimiter(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> String {
    let mut delimiter = String::new();
    let mut quote: Option<char> = None;
    while let Some(&(_, c)) = chars.peek() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => delimiter.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => {}
            (None, c) if c.is_whitespace() || ";&|()<>`".contains(c) => break,
            (None, c) => delimiter.push(c),
        }
        chars.next();
    }
    delimiter
}

/// A here-document's body in `line`, from `start` to the line that is
/// `delimiter` alone (after its leading tabs when `tabs`, and a Windows
/// line end), and where the line goes on after that one; to the end of
/// `line` when no line is.
fn here_body(line: &str, start: usize, delimiter: &str, tabs: bool) -> (Range<usize>, usize) {
    let mut at = start;
    while at < line.len() {
        let end = line[at..].find('\n').map_or(line.len(), |n| at + n);
        let text = line[at..end].strip_suffix('\r').unwrap_or(&line[at..end]);
        let text = if tabs {
            text.trim_start_matches('\t')
        } else {
            text
        };
        if text == delimiter {
            return (start..at, (end + 1).min(line.len()));
        }
        at = end + 1;
    }
    (start..line.len(), line.len())
}

/// Whether a command's here-document is the script it runs: a shell
/// ([`SHELLS`]) told to read its commands from its input (`-s`), or given
/// neither a `-c` script nor a script file, does (`bash <<EOF`).
fn reads_script(words: &[Word]) -> bool {
    let words: Vec<String> = words
        .iter()
        .filter(|word| !word.redirected)
        .map(|word| word.text.clone())
        .collect();
    let Some(start) = program_start(&words) else {
        return false;
    };
    let (name, args) = (program_name(&words[start]), &words[start + 1..]);
    if !SHELLS.contains(&name.as_str()) || shell_script(args).is_some() {
        return false;
    }
    let flags = args
        .iter()
        .take_while(|arg| arg.starts_with('-') && !arg.starts_with("--"));
    flags.clone().any(|flag| flag.contains('s'))
        || INTERPRETERS
            .iter()
            .find(|interpreter| interpreter.name == name)
            .is_none_or(|interpreter| script_file(interpreter, args).is_none())
}

/// Where a segment's program is: its first word that neither sets a
/// variable for it nor wraps it ([`WRAPPERS`]).
fn program_start(words: &[String]) -> Option<usize> {
    words
        .iter()
        .position(|word| !assignment(word) && !WRAPPERS.contains(&word.as_str()))
}

/// Whether `word` sets a variable for the command after it (`RUST_LOG=1`).
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// A shell command line's terms, as parts (see [`Terms::parts`]): each
/// segment is one, except that a reading command ([`READERS`]) is one per
/// path it names, each a read of that path alone, a writing one
/// ([`WRITERS`]) one per file it writes, and a shell's `-c` script is
/// split as a line of its own. Each file a redirection reads or writes is
/// a part of its own too, as the file tools' step on that file is.
fn command_terms(line: &str, stems: &mut Stems, terms: &mut Terms) {
    for Segment { words, files } in segments(line) {
        for file in &files {
            file_part(file, stems, terms);
        }
        let Some(start) = program_start(&words) else {
            continue;
        };
        let (program, args) = (words[start].as_str(), &words[start + 1..]);
        let name = program_name(program);
        if SHELLS.contains(&name.as_str())
            && let Some(script) = shell_script(args)
        {
            command_terms(script, stems, terms);
            continue;
        }
        if let Some(interpreter) = INTERPRETERS.iter().find(|i| i.name == name)
            && let Some(at) = script_file(interpreter, args)
        {
            // The script as if it ran alone, beside the interpreter's own
            // part, so a note on either matches as before.
            part_terms(
                &args[at],
                args[at + 1..].iter().map(String::as_str),
                stems,
                terms,
            );
        }
        let paths: Vec<usize> =
            if READERS.contains(&name.as_str()) || WRITERS.contains(&name.as_str()) {
                (0..args.len())
                    .filter(|&i| path_argument(&args[i]))
                    .collect()
            } else {
                Vec::new()
            };
        if paths.is_empty() {
            part_terms(program, args.iter().map(String::as_str), stems, terms);
            continue;
        }
        for &path in &paths {
            file_part(&args[path], stems, terms);
        }
    }
}

/// Adds `path`, a file a command reads or writes, to `terms` as a part of
/// its own: a step on that file, as the read or write tool's is. A device
/// (`/dev/null`) and a word that holds no anchor name no file.
fn file_part(path: &str, stems: &mut Stems, terms: &mut Terms) {
    if path.starts_with(DEVICES) || !path_argument(path) {
        return;
    }
    let mut part = Terms {
        file: true,
        ..Terms::default()
    };
    path_terms(path, stems, &mut part);
    terms.add_part(part);
}

/// Where a command's devices are, which a redirection names and no work
/// is about: `2>/dev/null` names no file, or a memory holding the anchor
/// `null` would come after every command that silences its errors.
const DEVICES: &str = "/dev/";

/// The script a shell's arguments run: the word after its first short
/// flags that hold `c` (`-c`, `-lc`), before any other argument.
fn shell_script(args: &[String]) -> Option<&str> {
    let flags = args
        .iter()
        .take_while(|arg| arg.starts_with('-') && !arg.starts_with("--"));
    let at = flags.clone().position(|flag| flag.contains('c'))?;
    args.get(at + 1).map(String::as_str)
}

/// Where in an interpreter's arguments the script file it runs is: the
/// first word after its flags, when it is a path; none after a flag that
/// takes code or a module ([`Interpreter::takes_code`]), or when that word is no path
/// (a flag's value, a subcommand).
fn script_file(interpreter: &Interpreter, args: &[String]) -> Option<usize> {
    for (at, arg) in args.iter().enumerate() {
        if arg.starts_with('-') {
            if interpreter.takes_code(arg) {
                return None;
            }
            continue;
        }
        return (!arg.contains(char::is_whitespace) && passages::path_like(arg)).then_some(at);
    }
    None
}

/// A program's name as a memory names it: the file name of its path,
/// split at `/` or `\`, lowercased and without [`WINDOWS_PROGRAM`], so
/// `C:\tools\rg.exe` is `rg` and reads as `rg` does.
fn program_name(program: &str) -> String {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_lowercase();
    match name.strip_suffix(WINDOWS_PROGRAM) {
        Some(stem) if !stem.is_empty() => stem.to_owned(),
        _ => name,
    }
}

/// The extension a Windows program's file has, which a command may write
/// and a memory leaves out (`cargo.exe publish` is `cargo publish`).
const WINDOWS_PROGRAM: &str = ".exe";

/// Whether a reading command's argument is a path it reads: no flag, no
/// quoted text, and an anchor of its own.
fn path_argument(arg: &str) -> bool {
    !arg.starts_with('-')
        && !arg.contains(char::is_whitespace)
        && !passages::anchors(arg).is_empty()
}

/// One part's terms, added to `terms` and kept as one of its parts: the
/// program with its subcommand, as a memory's code span names them
/// (`cargo publish`), and its path arguments, as anchors and as words.
/// The subcommand is the first word argument after the program and its
/// flags that is no path, so `cargo --locked test` names `cargo test` and
/// `git -C ../repo push` names `git push`. A flag's word value is taken
/// for the subcommand (`kubectl -n prod apply` names `kubectl prod`): the
/// delivery benchmark (#125) found this rule's deliveries 0.90 needed then
/// and 0.98 about the action at 1,000 memories, against 0.85 and 0.92 for
/// taking the first word some memory names, and 0.86 and 0.94 for the word
/// after a flag, which mistake a later argument for the subcommand more
/// often than they find a flag's value. An argument holding a space is
/// quoted text, such as a commit message, and adds nothing: its words and
/// any path it mentions are no part of the step.
fn part_terms<'a>(
    program: &str,
    args: impl Iterator<Item = &'a str>,
    stems: &mut Stems,
    terms: &mut Terms,
) {
    let args: Vec<&str> = args
        .filter(|arg| !arg.contains(char::is_whitespace))
        .collect();
    let mut part = Terms::default();
    let written = program;
    let program = program
        .strip_prefix("./")
        .or_else(|| program.strip_prefix(".\\"))
        .unwrap_or(program);
    let name = program_name(program);
    let sub = args.first().filter(|arg| !arg.starts_with('-'));
    // `rg` and `rg.exe` are one program (#229), whichever a memory or the
    // command writes: its name with and without [`WINDOWS_PROGRAM`] are
    // spellings of one term, counted once, and its words are the name's.
    let windows = program
        .to_lowercase()
        .strip_suffix(WINDOWS_PROGRAM)
        .is_some_and(|stem| !stem.is_empty());
    let named = [name.clone(), format!("{name}{WINDOWS_PROGRAM}")];
    part.anchors.insert(name.clone());
    if passages::path_like(written) && !windows {
        // A program run by its path, `tools/x.sh`, `./tools/x.sh` or a
        // script at the root (`./build.sh`, `build.sh` an interpreter
        // runs), is what a memory names either way (#220): the spellings,
        // as written and with and without `./`, are one path, counted
        // once.
        let mut spelled = passages::anchors(written);
        spelled.extend(passages::anchors(program));
        spelled.extend(passages::anchors(&format!("./{program}")));
        part.anchors.extend(spelled.iter().cloned());
        part.paths.push(spelled);
    } else {
        // A bare program, or one run by its `.exe`: written with a folder
        // (`C:\tools\rg.exe`) it keeps that path as one more spelling.
        let mut spelled = passages::anchors(written);
        spelled.extend(passages::anchors(program));
        spelled.extend(named);
        part.anchors.extend(spelled.iter().cloned());
        part.paths.push(spelled);
    }
    part.words(stems, if windows { &name } else { program });
    if let Some(sub) = sub {
        part.words(stems, sub);
    }
    if let Some(word) = args
        .iter()
        .find(|arg| word_argument(arg) && !arg.contains('/'))
    {
        part.anchors.insert(format!("{name} {word}").to_lowercase());
    }
    for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
        path_terms(arg, stems, &mut part);
    }
    terms.add_part(part);
}

/// An argument's terms, added to `part`: its anchors and words, and for a
/// path what a read of it names too, its tails, its folders and its
/// language, all spelling one path (see [`Terms::paths`]).
fn path_terms(arg: &str, stems: &mut Stems, part: &mut Terms) {
    let found = passages::anchors(arg);
    if found.is_empty() {
        return;
    }
    part.words(stems, arg);
    part.anchors.extend(found.iter().cloned());
    if passages::path_like(arg) {
        let mut read = Terms::default();
        file_terms(arg, stems, &mut read);
        part.stems.extend(read.stems);
        let mut spelled = found;
        spelled.extend(read.anchors);
        part.anchors.extend(spelled.iter().cloned());
        part.paths.push(spelled);
    }
}

/// The first of the path spellings joined with `p`: `joined` holds, for
/// each, one joined with it that comes no later.
fn root(joined: &[usize], mut p: usize) -> usize {
    while joined[p] != p {
        p = joined[p];
    }
    p
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

/// The terms `action` adds to a window, outside one: what a hook keeps of
/// an action another hook's window takes in later.
pub(crate) fn terms_of(action: &Action) -> Terms {
    terms(action, &mut Stems::new())
}

/// The terms `action` adds to the window.
pub(in crate::memory) fn terms(action: &Action, stems: &mut Stems) -> Terms {
    let mut terms = Terms::default();
    match action.kind {
        Kind::Command => command_terms(&action.text, stems, &mut terms),
        // A patch names several files, one per line.
        Kind::File => {
            for path in action.text.lines() {
                file_terms(path, stems, &mut terms);
            }
        }
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
    pub(crate) share_floor: f64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            window_actions: WINDOW_ACTIONS,
            half_life: WINDOW_HALF_LIFE,
            share_floor: WINDOW_SHARE_FLOOR,
        }
    }
}

/// An agent context's recent actions, newest last, with the stems of the
/// words they used.
pub(crate) struct Window {
    tuning: Tuning,
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the delivery benchmark pushes actions; the hooks push the terms they keep"
        )
    )]
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
    /// [`WINDOW_ACTIONS`] falls out. Returns the terms it added.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the delivery benchmark pushes actions; the hooks push the terms they keep"
        )
    )]
    pub(crate) fn push(&mut self, action: &Action) -> &Terms {
        let terms = terms(action, &mut self.stems);
        self.push_terms(action.kind, terms, action.failed)
    }

    /// Adds an action already taken to terms, as a hook reads it back.
    pub(crate) fn push_terms(&mut self, kind: Kind, terms: Terms, failed: bool) -> &Terms {
        self.actions.push_back((kind, terms, failed));
        while self.actions.len() > self.tuning.window_actions {
            self.actions.pop_front();
        }
        let (_, terms, _) = self.actions.back().expect("an action was just added");
        terms
    }
}

/// How the index keys a term: a stem or an anchor, as its hex token.
pub(in crate::memory) fn key(anchor: bool, term: &str) -> Vec<u8> {
    let mut key = vec![prefix(anchor)];
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
    /// The parts of the action that fired holding it, a bit each.
    parts: u64,
    /// The parts of the action that fired holding it as an anchor.
    anchor_parts: u64,
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
    /// most, which a command's bars test. For a line of several parts, the
    /// best share among the parts whose anchors it holds (else whose terms
    /// it holds), each counting the window's other actions and the part's
    /// own terms against what that part could score at most: as if the
    /// part had run alone.
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
    /// It holds an anchor of the command that fired, other than a read of
    /// a file, which holds nothing back.
    pub(crate) command: bool,
    /// The action that fired is a shell command and the passage holds an
    /// anchor of a part of it that is no read of a file, so the command
    /// bars apply; a passage only reads reach is tested as after the read
    /// tool, on `mixed`.
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

/// The rows holding one term, with the field holding it in each, and how
/// many memories hold it.
struct Found {
    holders: usize,
    rows: Vec<(Row, Field)>,
}

impl DeliveryIndex {
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
}

/// Where a ranking reads its postings and passages: one delivery index,
/// or a base with the overlay made for it.
trait Lookup {
    /// How many memories the rows come from.
    fn memories(&self) -> usize;
    /// The rows holding `key`, each with the field holding it there, and
    /// how many memories hold it.
    fn postings(&self, key: &[u8]) -> Result<Option<Found>, Error>;
    /// The first passage row of memory `m`.
    fn first_passage(&self, m: u32) -> Result<Option<Row>, Error>;
}

impl Lookup for DeliveryIndex {
    fn memories(&self) -> usize {
        self.memories
    }
    fn postings(&self, key: &[u8]) -> Result<Option<Found>, Error> {
        DeliveryIndex::postings(self, key)
    }
    fn first_passage(&self, m: u32) -> Result<Option<Row>, Error> {
        DeliveryIndex::first_passage(self, m)
    }
}

/// A base with its overlay, as one index: the overlay's memories numbered
/// after the base's, and the base's rows of the memories the overlay's skip
/// list names left out, so a changed memory is matched on its new words
/// only and a forgotten one not at all.
struct Joined<'a> {
    base: &'a DeliveryIndex,
    overlay: &'a DeliveryIndex,
    /// The base's positions of the memories the overlay skips.
    skipped: HashSet<u32>,
}

impl Joined<'_> {
    /// The overlay's memory `m` as numbered here.
    fn shifted(&self, m: u32) -> u32 {
        m + self.base.memories as u32
    }
}

impl Lookup for Joined<'_> {
    fn memories(&self) -> usize {
        self.base.memories - self.skipped.len() + self.overlay.memories
    }
    fn postings(&self, key: &[u8]) -> Result<Option<Found>, Error> {
        let mut rows = Vec::new();
        if let Some(found) = self.base.postings(key)? {
            rows.extend(
                found
                    .rows
                    .into_iter()
                    .filter(|(row, _)| !self.skipped.contains(&row.memory)),
            );
        }
        if let Some(found) = self.overlay.postings(key)? {
            rows.extend(found.rows.into_iter().map(|(row, field)| {
                (
                    Row {
                        memory: self.shifted(row.memory),
                        ..row
                    },
                    field,
                )
            }));
        }
        // Each term's rarity among the memories kept, not the base's count.
        let holders: HashSet<u32> = rows.iter().map(|(row, _)| row.memory).collect();
        Ok((!rows.is_empty()).then_some(Found {
            holders: holders.len(),
            rows,
        }))
    }
    fn first_passage(&self, m: u32) -> Result<Option<Row>, Error> {
        let offset = self.base.memories as u32;
        if m < offset {
            return self.base.first_passage(m);
        }
        Ok(self.overlay.first_passage(m - offset)?.map(|row| Row {
            memory: self.shifted(row.memory),
            ..row
        }))
    }
}

impl View {
    /// The passages `window` matches in the base and its overlay, as
    /// [`ranked`] ranks one index; each [`Matched::memory`] is a position
    /// [`View::id`] names.
    pub(crate) fn rank(&self, window: &Window) -> Result<Vec<Matched>, Error> {
        let Some(overlay) = &self.overlay else {
            return ranked(&self.base, window);
        };
        let skip: HashSet<&str> = overlay.header.skip.iter().map(String::as_str).collect();
        let mut skipped = HashSet::new();
        if !skip.is_empty() {
            for m in 0..self.base.memories {
                if skip.contains(self.base.id(m)?) {
                    skipped.insert(m as u32);
                }
            }
        }
        ranked(
            &Joined {
                base: &self.base,
                overlay,
                skipped,
            },
            window,
        )
    }

    /// The id of the memory at position `m` of a [`View::rank`] match.
    pub(crate) fn id(&self, m: usize) -> Result<&str, Error> {
        match &self.overlay {
            Some(overlay) if m >= self.base.memories => overlay.id(m - self.base.memories),
            _ => self.base.id(m),
        }
    }

    /// Whether the memory at position `m` of a [`View::rank`] match holds
    /// until a date at or before `now`, both in the stored form: ended,
    /// though no marker moved, so no push names it (#121). A base's memory
    /// a match names is one its overlay doesn't hide, so the base's date
    /// is its own.
    pub(crate) fn past_date(&self, m: usize, now: &str) -> Result<bool, Error> {
        let (file, id) = match &self.overlay {
            Some(overlay) if m >= self.base.memories => {
                (overlay, overlay.id(m - self.base.memories)?)
            }
            _ => (&self.base, self.base.id(m)?),
        };
        let dates = &file.header.dates;
        Ok(dates
            .binary_search_by(|(dated, _)| dated.as_str().cmp(id))
            .is_ok_and(|at| dates[at].1.as_str() <= now))
    }
}

/// The passages `window` matches, one per memory (its best), strongest
/// first, each scored as search scores a topic: what slot 0 holds plus
/// the best passage's own terms, each by its field's weight, its rarity
/// among the indexed memories and its age in the window. A memory's
/// other passages add nothing, as in search (#124), so a long memory
/// matching in many places does not outrank a short one matching as
/// well in one. Rarity is divided by the rarest possible; that changes
/// the scale only, not the order, and search's own score is untouched.
fn ranked(source: &impl Lookup, window: &Window) -> Result<Vec<Matched>, Error> {
    let mut wanted: BTreeMap<Vec<u8>, Wanted> = BTreeMap::new();
    let newest = window.actions.len().saturating_sub(1);
    let on_command = window
        .actions
        .back()
        .is_some_and(|(kind, _, _)| *kind == Kind::Command);
    let fired_parts = window
        .actions
        .back()
        .map_or(&[][..], |(_, terms, _)| terms.parts.as_slice());
    let parts = fired_parts.len().clamp(1, MAX_PARTS);
    let fired_paths = window
        .actions
        .back()
        .map_or(&[][..], |(_, terms, _)| terms.paths.as_slice());
    // Spellings shared by two of them spell one path: a script the line
    // runs is its own part's program and its interpreter's argument.
    let mut path_of: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut joined: Vec<usize> = (0..fired_paths.len()).collect();
    for (p, spelled) in fired_paths.iter().enumerate() {
        for anchor in spelled {
            let first = *path_of.entry(key(true, anchor)).or_insert(p);
            let (a, b) = (root(&joined, first), root(&joined, p));
            joined[a.max(b)] = a.min(b);
        }
    }
    for p in path_of.values_mut() {
        *p = root(&joined, *p);
    }
    // Each path's best spelling some memory holds, and its parts.
    let mut paths: Vec<(Option<f64>, u64)> = vec![(None, 0); fired_paths.len()];
    let mut part_of: HashMap<Vec<u8>, u64> = HashMap::new();
    // The parts that read a file.
    let mut reads = 0u64;
    for (i, part) in fired_parts.iter().enumerate() {
        let bit = 1u64 << i.min(MAX_PARTS - 1);
        if part.file {
            reads |= bit;
        }
        let keys = part
            .stems
            .iter()
            .map(|stem| key(false, stem))
            .chain(part.anchors.iter().map(|anchor| key(true, anchor)));
        for key in keys {
            *part_of.entry(key).or_default() |= bit;
        }
    }
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
            // An action of one part, or read back, is all part 0.
            let bits = if fired {
                part_of.get(&key).copied().unwrap_or(1)
            } else {
                0
            };
            let w = wanted.entry(key).or_default();
            w.weight = w.weight.max(weight);
            w.fired |= fired;
            w.failed |= fired && *failed;
            w.anchored |= fired && anchor;
            w.command |= fired && anchor && *kind == Kind::Command;
            w.parts |= bits;
            if anchor {
                w.anchor_parts |= bits;
            }
        }
    }
    let all = source.memories() as f64;
    let rarest = rarity(all, 1.0);
    /// One passage's hits: its row, its gain, what it holds of the firing
    /// action and its score split by part.
    type Passage = (Row, Gain, Wanted, Split);
    /// One memory's hits: slot 0's score, flags and split, and each
    /// passage's.
    #[derive(Default)]
    struct Hits {
        whole: Gain,
        whole_wanted: Wanted,
        whole_split: Split,
        passages: BTreeMap<u16, Passage>,
    }
    let mut found: HashMap<u32, Hits> = HashMap::new();
    // What the action that fired could score at most, as a whole and by
    // part: each of its terms once as text, a term no memory holds as rare
    // as one memory's, and each path it names once (see `Terms::paths`).
    let mut possible = 0.0;
    let mut possible_parts = vec![0.0; parts];
    for (key, want) in &wanted {
        let postings = source.postings(key)?;
        let anchor = key.first() == Some(&b'a');
        if want.fired {
            let holders = postings.as_ref().map_or(1, |f| f.holders.max(1));
            let base = if anchor {
                ANCHOR_WEIGHT
            } else {
                weight(Field::Body)
            };
            let most = base * rarity(all, holders as f64) / rarest;
            if let Some(&p) = path_of.get(key) {
                let (best, bits) = &mut paths[p];
                *bits |= want.parts;
                if postings.is_some() {
                    *best = Some(best.map_or(most, |b| b.max(most)));
                }
            } else {
                possible += most;
                for (part, sum) in possible_parts.iter_mut().enumerate() {
                    if want.parts & (1 << part) != 0 {
                        *sum += most;
                    }
                }
            }
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
            let store = base * rarity(all, held) / rarest;
            Gain {
                store,
                own: if want.fired { store } else { 0.0 },
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
            let gain = worth(field);
            let (total, flags, split) = if row.slot == 0 {
                (
                    &mut hits.whole,
                    &mut hits.whole_wanted,
                    &mut hits.whole_split,
                )
            } else {
                let entry = hits.passages.entry(row.slot).or_insert((
                    row,
                    Gain::default(),
                    Wanted::default(),
                    Split::default(),
                ));
                (&mut entry.1, &mut entry.2, &mut entry.3)
            };
            total.add(gain);
            merge(flags, want);
            if parts > 1 {
                split.add(want.parts, gain.store);
            }
        }
    }
    for (p, (best, bits)) in paths.into_iter().enumerate() {
        if joined[p] != p {
            continue;
        }
        // As written, a path no memory holds is as rare as one memory's.
        let most = best.unwrap_or(ANCHOR_WEIGHT * rarity(all, 1.0) / rarest);
        possible += most;
        for (part, sum) in possible_parts.iter_mut().enumerate() {
            if bits & (1 << part) != 0 {
                *sum += most;
            }
        }
    }
    let mut matched = Vec::with_capacity(found.len());
    for (memory, hits) in found {
        let best = hits
            .passages
            .values()
            .fold(None::<&Passage>, |best, candidate| match best {
                Some(b) if b.1.store >= candidate.1.store => Some(b),
                _ => Some(candidate),
            });
        let (row, gain, own, own_split) = match best {
            Some((row, gain, own, split)) => (*row, *gain, *own, split.clone()),
            None => match source.first_passage(memory)? {
                Some(row) => (row, Gain::default(), Wanted::default(), Split::default()),
                None => continue,
            },
        };
        let mut flags = hits.whole_wanted;
        merge(&mut flags, &own);
        let mut total = hits.whole;
        total.add(gain);
        let mut split = hits.whole_split;
        split.join(own_split);
        // Reached through reads of files alone, as after the read tool.
        let read_only = flags.anchor_parts != 0 && flags.anchor_parts & !reads == 0;
        matched.push(Matched {
            memory: memory as usize,
            slot: i64::from(row.slot),
            start: row.start as usize,
            length: row.length as usize,
            score: total.store,
            mixed: total.mixed,
            share: if parts > 1 && flags.parts != 0 {
                let held = if flags.anchor_parts != 0 {
                    flags.anchor_parts
                } else {
                    flags.parts
                };
                split.best_share(held, &possible_parts, window.tuning.share_floor)
            } else if possible > 0.0 {
                share(
                    total.own,
                    total.store - total.own,
                    possible,
                    window.tuning.share_floor,
                )
            } else {
                0.0
            },
            fired: flags.fired,
            failed: flags.failed,
            anchored: flags.anchored,
            command: flags.command && !read_only,
            on_command: on_command && !read_only,
            fewest: flags.fewest,
        });
    }
    matched.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.memory.cmp(&b.memory)));
    Ok(matched)
}

/// A match's score by the store's rarity, and free of the store's size
/// (see [`Matched::mixed`]).
#[derive(Clone, Copy, Default)]
struct Gain {
    store: f64,
    /// What of `store` the terms of the action that fired hold.
    own: f64,
    mixed: f64,
}

impl Gain {
    fn add(&mut self, other: Gain) {
        self.store += other.store;
        self.own += other.own;
        self.mixed += other.mixed;
    }
}

/// A passage's share of `most`, the most the action that fired could
/// score: what its own terms hold, `own`, and what the window's older
/// actions add, `others`, once its own reach `floor`
/// ([`WINDOW_SHARE_FLOOR`]).
fn share(own: f64, others: f64, most: f64, floor: f64) -> f64 {
    let alone = own / most;
    if alone < floor {
        alone
    } else {
        (own + others) / most
    }
}

/// A match's score by the store's rarity, split by the parts of the line
/// that fired: what the window's other actions add, and what each term of
/// the line adds with the parts holding it. Kept only for a line of
/// several parts.
#[derive(Clone, Default)]
struct Split {
    others: f64,
    terms: Vec<(u64, f64)>,
}

impl Split {
    fn add(&mut self, parts: u64, store: f64) {
        if parts == 0 {
            self.others += store;
        } else {
            self.terms.push((parts, store));
        }
    }

    fn join(&mut self, other: Split) {
        self.others += other.others;
        self.terms.extend(other.terms);
    }

    /// The best share among the parts in `held`: the part's own terms,
    /// and the window's other actions once those reach `floor` (see
    /// [`share`]), against `possible`, its most.
    fn best_share(&self, held: u64, possible: &[f64], floor: f64) -> f64 {
        let mut best = 0.0f64;
        for (part, most) in possible.iter().enumerate() {
            let bit = 1u64 << part;
            if held & bit == 0 || *most <= 0.0 {
                continue;
            }
            let own: f64 = self
                .terms
                .iter()
                .filter(|(parts, _)| parts & bit != 0)
                .map(|(_, store)| store)
                .sum();
            best = best.max(share(own, self.others, *most, floor));
        }
        best
    }
}

/// Adds what `other` holds of the firing action to `into`.
fn merge(into: &mut Wanted, other: &Wanted) {
    into.fired |= other.fired;
    into.parts |= other.parts;
    into.anchor_parts |= other.anchor_parts;
    into.failed |= other.failed;
    into.anchored |= other.anchored;
    into.command |= other.command;
    if other.fewest > 0 && (into.fewest == 0 || other.fewest < into.fewest) {
        into.fewest = other.fewest;
    }
}

/// `matched`'s passage of a memory whose content and context are
/// `content` and `context`: empty when the memory changed since the index
/// was made and the passage no longer falls on its text.
pub(crate) fn cut<'a>(content: &'a str, context: &'a str, matched: &Matched) -> &'a str {
    let source = if matched.slot == CONTEXT_SLOT {
        context
    } else {
        content
    };
    source
        .get(matched.start..matched.start + matched.length)
        .unwrap_or("")
        .trim()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests fail loudly on a broken setup")]
    use super::*;
    use crate::memory::delivery::{ROW_BYTES, Source, SourceRow};
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
    fn an_interpreter_runs_the_script_file_after_its_flags_only() {
        let words = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let file = |interpreter: &str, line: &str| {
            let interpreter = INTERPRETERS.iter().find(|i| i.name == interpreter).unwrap();
            script_file(interpreter, &words(line))
        };
        assert_eq!(file("python3", "tools/build.py"), Some(0));
        assert_eq!(file("python3", "-u -X tools/build.py --fast"), Some(2));
        assert_eq!(file("bash", "./tools/step2.sh"), Some(0));
        // A flag that takes no value: a shell's `-e`, perl's `-p`.
        assert_eq!(file("sh", "-e tools/step2.sh --all"), Some(1));
        assert_eq!(file("perl", "-p tools/fix.pl"), Some(1));
        // Code or a module, not a file.
        for (interpreter, given) in [
            ("python3", "-m tools.lint"),
            ("python3", "-c tools/x.py"),
            ("node", "-e app.js"),
            ("node", "-p ./package.json"),
            ("node", "--print ./package.json"),
            ("php", "-r tools/x.php"),
            ("perl", "-E say.pl"),
            ("pwsh", "-Command x.ps1"),
            ("pwsh", "-EC x.ps1"),
        ] {
            assert_eq!(file(interpreter, given), None, "{interpreter} {given}");
        }
        // Only PowerShell reads its flags in any case.
        assert_eq!(file("python3", "-C tools/x.py"), Some(1));
        // A word that is no path: a flag's value or a subcommand.
        assert_eq!(file("python3", "run tools/build.py"), None);
        // The script's part is the script alone: `./` and the interpreter
        // gone, its own arguments kept.
        let terms = of(Kind::Command, "python3 -u ./tools/build.py --fast");
        let script = terms
            .parts
            .iter()
            .find(|part| !part.anchors.contains("python3"))
            .unwrap();
        assert!(script.anchors.contains("tools/build.py"), "{script:?}");
        assert!(
            terms
                .parts
                .iter()
                .any(|part| part.anchors.contains("python3"))
        );
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
        let ids: Vec<String> = (0..memories.len())
            .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
            .collect();
        let with: Vec<(&str, &str, &str)> = ids
            .iter()
            .zip(memories)
            .map(|(id, (title, content))| (id.as_str(), *title, *content))
            .collect();
        DeliveryIndex::read(DeliveryIndex::write(&sources(&with)).unwrap()).unwrap()
    }

    /// The rows of memories given as id, title and content.
    fn sources(memories: &[(&str, &str, &str)]) -> Vec<Source> {
        let mut stems = Stems::new();
        memories
            .iter()
            .map(|(id, title, content)| {
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
                    id: (*id).to_owned(),
                    rows,
                }
            })
            .collect()
    }

    /// A view of `base` with an overlay of `changed` memories hiding
    /// `skip`, as a refresh writes one.
    fn view(base: &[(&str, &str, &str)], changed: &[(&str, &str, &str)], skip: &[&str]) -> View {
        use crate::memory::delivery::{Header, Part};
        let base = DeliveryIndex::read(DeliveryIndex::write(&sources(base)).unwrap()).unwrap();
        let header = Header {
            part: Part::Overlay,
            position: "later".into(),
            made_for: base.header.position.clone(),
            projects: base.header.projects.clone(),
            skip: skip.iter().map(|id| (*id).to_owned()).collect(),
            dates: Vec::new(),
        };
        let overlay =
            DeliveryIndex::read(DeliveryIndex::write_with(&header, &sources(changed)).unwrap())
                .unwrap();
        View::new(base, Some(overlay)).unwrap()
    }

    #[test]
    fn a_view_matches_the_overlays_rows_for_what_changed_and_hides_what_it_skips() {
        const A: &str = "e0000001-0000-4000-8000-000000000001";
        const B: &str = "e0000002-0000-4000-8000-000000000002";
        const C: &str = "e0000003-0000-4000-8000-000000000003";
        const D: &str = "e0000004-0000-4000-8000-000000000004";
        let publish = "Run `cargo publish --dry-run` before `cargo publish`.";
        let base = [
            (A, "Publish note", publish),
            (B, "Second publish note", publish),
            (C, "Parser tables", "Keep `tables.rs` sorted."),
        ];
        let window = |command: &str| {
            let mut window = Window::default();
            window.push(&action(Kind::Command, command));
            window
        };
        // The memories pushed, by id.
        let ids = |view: &View, found: &[Matched]| -> Vec<String> {
            found
                .iter()
                .filter(|m| m.push())
                .map(|m| view.id(m.memory).unwrap().to_owned())
                .collect()
        };
        // Without changes both notes match, each anchor held by two.
        let plain = view(&base, &[], &[]);
        let found = plain.rank(&window("cargo publish")).unwrap();
        assert_eq!(ids(&plain, &found), [A, B]);
        assert_eq!(found[0].fewest, 2);
        // A changed to no longer name it, B forgotten, D new: only D, and
        // the anchor's holders are counted over the memories kept.
        let changed = view(
            &base,
            &[
                (A, "Publish note", "Run `git tag` first."),
                (D, "Fourth publish note", publish),
            ],
            &[A, B],
        );
        let found = changed.rank(&window("cargo publish")).unwrap();
        assert_eq!(ids(&changed, &found), [D]);
        assert_eq!(found[0].fewest, 1);
        // A's new words match it, by its id.
        assert_eq!(
            ids(&changed, &changed.rank(&window("git tag v1")).unwrap()),
            [A]
        );
        assert_eq!(
            ids(&plain, &plain.rank(&window("git tag v1")).unwrap()),
            [] as [&str; 0]
        );
        // C, unchanged, still matches from the base.
        let mut parser = Window::default();
        parser.push(&action(Kind::File, "src/tables.rs"));
        assert_eq!(ids(&changed, &changed.rank(&parser).unwrap()), [C]);
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
        ranked(index, &window).unwrap()
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
        assert!(matches!(ranked(&damaged, &window), Err(Error::Format)));
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
    fn a_path_a_line_names_twice_counts_once() {
        let index = store(
            "deploy",
            &[(
                "Copying here",
                "`cp` here is the GNU one, which keeps links.",
            )],
        );
        let share = |line: &str| {
            let found = rank(&index, &[action(Kind::Command, line)]);
            by_id(&index, &found, 40).unwrap().share
        };
        let once = share("cp tools/a.sh");
        assert!(once > 0.0);
        assert_eq!(share("cp tools/a.sh tools/a.sh"), once);
        // A script its interpreter runs is its own part's program and the
        // interpreter's argument: one path, so a note on the interpreter
        // scores as if the script were named once.
        let index = store(
            "deploy",
            &[(
                "Python here",
                "`python3` here is 3.13, which the tools need.",
            )],
        );
        let share = |line: &str| {
            let found = rank(&index, &[action(Kind::Command, line)]);
            by_id(&index, &found, 40).unwrap().share
        };
        assert_eq!(
            share("python3 tools/build.py"),
            share("python3 tools/build.py tools/build.py")
        );
    }

    #[test]
    fn a_program_scores_the_same_with_its_exe_or_without() {
        let shares = |content: &str, lines: &[&str]| -> Vec<f64> {
            let index = store("deploy", &[("A note", content)]);
            lines
                .iter()
                .map(|line| {
                    let found = rank(&index, &[action(Kind::Command, line)]);
                    by_id(&index, &found, 40).unwrap().share
                })
                .collect()
        };
        // A note on the program: its `.exe` adds nothing to match.
        let rg = shares(
            "`rg` here is ripgrep 14, which skips ignored files.",
            &["rg -n foo", "rg.exe -n foo", "RG.EXE -n foo"],
        );
        assert_eq!(rg[1], rg[0], "{rg:?}");
        assert_eq!(rg[2], rg[0], "{rg:?}");
        let git = shares(
            "Run `git status` before a commit to see untracked files.",
            &["git status", "git.exe status"],
        );
        assert_eq!(git[1], git[0], "{git:?}");
        // A note writing `.exe` matches both spellings as one program.
        let exe = shares(
            "`rg.exe` here is ripgrep 14, which skips ignored files.",
            &["rg -n foo", "rg.exe -n foo"],
        );
        assert_eq!(exe[0], exe[1], "{exe:?}");
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

    #[test]
    fn a_line_is_a_part_per_command_and_per_file_a_reader_reads() {
        let parts = |line: &str| -> Vec<BTreeSet<String>> {
            of(Kind::Command, line)
                .parts
                .into_iter()
                .map(|part| part.anchors)
                .collect()
        };
        let chained = parts("cat RELEASING.md; echo ---; cat tools/package.py");
        assert_eq!(chained.len(), 3, "{chained:?}");
        assert!(chained[2].contains("tools/package.py") && !chained[2].contains("releasing.md"));
        // A reader's paths are a part each, with the program in each; any
        // other command's paths stay together.
        let read = parts("head -n 50 README.md src/a.py tools/package.py");
        assert_eq!(read.len(), 3, "{read:?}");
        // Each a read of its file alone, with no term of the program.
        assert!(read.iter().all(|part| !part.contains("head")), "{read:?}");
        assert!(read[2].contains("tools/package.py") && !read[2].contains("readme.md"));
        assert_eq!(parts("cp README.md src/a.py tools/").len(), 1);
        // Subshells, substitutions and backticks split; a redirection's `&`
        // does not, and the file a redirection names is a part of its own.
        for (line, count) in [
            ("(cd tools && make package)", 2),
            ("echo $(cat VERSION) `git rev-parse HEAD`", 3),
            ("cargo test 2>&1 | tail -5", 2),
            ("cargo build &> build.log", 2),
        ] {
            assert_eq!(parts(line).len(), count, "{line}: {:?}", parts(line));
        }
        // A shell's script is split as a line of its own.
        let shell = parts("bash -lc 'cargo publish && git push'");
        assert_eq!(shell.len(), 2, "{shell:?}");
        assert!(shell[0].contains("cargo publish") && shell[1].contains("git push"));
        // Every part's terms are in the line's.
        let terms = of(Kind::Command, "cat RELEASING.md; cat tools/package.py");
        for part in &terms.parts {
            assert!(terms.anchors.is_superset(&part.anchors));
            assert!(terms.stems.is_superset(&part.stems));
        }
    }

    /// A program given by its path, with either separator and Windows'
    /// `.exe`, is the program its file name names.
    #[test]
    fn a_program_given_by_its_path_is_named_by_its_file_name() {
        let read = of(
            Kind::Command,
            "C:/tools/rg.exe store src/store/cache.py tools/package.py",
        );
        assert_eq!(read.parts.len(), 2, "{read:?}");
        assert!(read.parts.iter().all(|part| part.file), "{read:?}");
        for line in [
            "C:/tools/cargo.exe publish",
            r"C:\tools\cargo.exe publish",
            "/usr/local/bin/cargo publish",
        ] {
            let terms = of(Kind::Command, line);
            assert!(terms.anchors.contains("cargo publish"), "{line}: {terms:?}");
        }
    }

    #[test]
    fn quoted_text_in_a_command_adds_nothing() {
        let terms = of(
            Kind::Command,
            "git commit -m \"Add one to notes.txt\n\nCo-Authored-By: someone\"",
        );
        assert!(terms.anchors.contains("git commit"), "{terms:?}");
        assert!(!terms.anchors.contains("notes.txt"), "{terms:?}");
        for word in ["add", "note", "author"] {
            assert!(!terms.stems.contains(word), "{word}: {terms:?}");
        }
        // A quoted path holds no space, and stays a path.
        let quoted = of(Kind::Command, "cat 'docs/notes.md'");
        assert!(quoted.anchors.contains("docs/notes.md"), "{quoted:?}");
    }

    #[test]
    fn a_note_on_one_part_of_a_long_line_is_pushed_as_after_that_part_alone() {
        let note = (
            "Packaging needs the key",
            "Run `python3 tools/package.py --key 4QF7`; without the key it builds an unsigned archive.",
        );
        let other = (
            "Cache layout",
            "src/store/cache.py keeps one file per entry under the cache folder.",
        );
        let index = store("deploy", &[note, other]);
        let lines = [
            "python3 tools/package.py",
            "cat RELEASING.md; echo ---; cat tools/package.py",
            "cat README.md src/a.py src/b.py src/c.py tools/package.py",
            "echo one >> notes.txt && python3 tools/package.py --dry-run",
            "git add notes.txt && git commit -m \"Add one\" && python3 tools/package.py",
            "(cd build && make) && python3 tools/package.py 2>&1 | tail -20",
        ];
        for line in lines {
            let found = rank(&index, &[action(Kind::Command, line)]);
            let hit = by_id(&index, &found, 40).unwrap();
            assert!(hit.push(), "{line}: {hit:?}");
            // A path the line never names pushes nothing.
            assert!(
                by_id(&index, &found, 41).is_none_or(|m| !m.push()),
                "{line}: {found:?}"
            );
        }
        // Reading the other note's file pushes it, and not the first.
        let found = rank(
            &index,
            &[action(
                Kind::Command,
                "cat README.md src/store/cache.py tests/test_store.py",
            )],
        );
        assert!(by_id(&index, &found, 41).unwrap().push(), "{found:?}");
        assert!(
            by_id(&index, &found, 40).is_none_or(|m| !m.push()),
            "{found:?}"
        );
    }

    #[test]
    fn a_path_a_command_names_reaches_what_a_read_of_it_reaches() {
        let folder = (
            "Store contract tests",
            "CI collects only `test_<name>_contract` functions for code under `src/store/`.",
        );
        let script = (
            "Packaging needs the key",
            "Run `python3 tools/package.py --key 4QF7`; without the key it builds an unsigned archive.",
        );
        let index = store("deploy", &[folder, script]);
        let read = rank(&index, &[action(Kind::File, "src/store/cache.py")]);
        assert!(by_id(&index, &read, 40).unwrap().push(), "{read:?}");
        for line in [
            // A file in the folder the note names, as a read of it finds.
            "cat src/store/cache.py",
            "head -20 ./src/store/cache.py",
            "sed -n 1,40p src/store/cache.py",
        ] {
            let found = rank(&index, &[action(Kind::Command, line)]);
            let hit = by_id(&index, &found, 40).unwrap();
            assert!(hit.push(), "{line}: {hit:?}");
        }
        // A path from elsewhere ends with the one the note names.
        for line in [
            "python3 ../ledger/tools/package.py",
            "cat README.md ../ledger/tools/package.py",
        ] {
            let found = rank(&index, &[action(Kind::Command, line)]);
            let hit = by_id(&index, &found, 41).unwrap();
            assert!(hit.push(), "{line}: {hit:?}");
        }
        // Another folder's file reaches neither.
        let found = rank(&index, &[action(Kind::Command, "cat src/parse/lexer.py")]);
        assert!(
            by_id(&index, &found, 40).is_none_or(|m| !m.push()),
            "{found:?}"
        );
        // Its tails and folders spell the one path it names; the program
        // is a term of its own (#229).
        let terms = of(Kind::Command, "cat src/store/cache.py");
        let file: Vec<_> = terms
            .paths
            .iter()
            .filter(|path| path.contains("cache.py"))
            .collect();
        assert_eq!(file.len(), 1, "{terms:?}");
        for anchor in [
            "src/store/cache.py",
            "store/cache.py",
            "cache.py",
            "src/store/",
            "src",
        ] {
            assert!(file[0].contains(anchor), "{anchor}: {terms:?}");
        }
        assert!(!file[0].contains("cat"), "{terms:?}");
    }

    #[test]
    fn a_read_through_the_shell_is_tested_as_a_read_and_holds_nothing_back() {
        let docs = (
            "Docs review line",
            "The docs site build rejects any page under `docs/` whose last line is not its review month.",
        );
        let publish = (
            "Dry run before publishing",
            "Run `cargo publish --dry-run` before `cargo publish`; a failed upload burns the version number.",
        );
        let index = store("deploy", &[docs, publish]);
        let read = rank(&index, &[action(Kind::File, "docs/guide.md")]);
        let by_read = by_id(&index, &read, 40).unwrap();
        assert!(by_read.push() && !by_read.hold(), "{by_read:?}");
        let found = rank(
            &index,
            &[action(Kind::Command, "cat docs/guide.md && cargo publish")],
        );
        // The read's passage, as after the read tool: pushed on `mixed`,
        // never held.
        let hit = by_id(&index, &found, 40).unwrap();
        assert!(
            hit.push() && !hit.on_command && !hit.command && !hit.hold(),
            "{hit:?}"
        );
        assert_eq!(hit.mixed, by_read.mixed, "{hit:?} {by_read:?}");
        // The command beside it keeps the command bars and may hold.
        let hit = by_id(&index, &found, 41).unwrap();
        assert!(hit.on_command && hit.push() && hit.hold(), "{hit:?}");
    }
}
