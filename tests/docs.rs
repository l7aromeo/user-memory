//! The docs and release notes (README.md, CONTRIBUTING.md, SECURITY.md,
//! docs/ and release-notes/) against the code, so docs that disagree with
//! the binary cannot be released: scripts/client-release runs this test
//! before it builds.
//!
//! - Every `user-memory …` command in a code block or inline code span must
//!   name subcommands the built binary has, with flags its `--help` lists.
//! - Every quoted output must come from a string in src/, install.sh or
//!   uninstall.sh.
//!
//! Marking quoted output, for whoever writes the docs:
//!
//! - A fenced block tagged `output` is checked line by line.
//! - An inline span in double backticks (``like this``) is a quoted message;
//!   it renders as plain code. A double-backtick span that holds a backtick
//!   is ordinary inline code, as Markdown means it, and is not checked.
//! - In a quote, `…` and `<placeholder>` stand for text left out.
//! - A line of an output block that shows a memory (an 8-character id) or a
//!   group heading (`  workflow (3)`) is data, not a message.
//! - Anything else in backticks or a fenced block is code: its
//!   `user-memory …` commands are checked.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

const BINARY: &str = env!("CARGO_BIN_EXE_user-memory");

/// A pattern's text must hold a word this long, so a pattern of holes and
/// punctuation such as `{}: {}` cannot match any line; `{}: OK` can.
const PATTERN_WORD: usize = 2;

/// A quoted fragment with fewer letters and digits than this, such as the
/// `(` left between two placeholders, says nothing to check.
const FRAGMENT_CHARS: usize = 3;

/// The most matching work one quote may take, in matcher steps. Counted,
/// not timed, so a slow machine fails nothing: a doc's quotes take at most
/// about 210,000 steps, and the longest No-project line the code can print
/// 3.2 million. A matcher that backtracked over hole boundaries took tens
/// of millions on one doc quote (17 s), so a slowdown that size fails here
/// instead of creeping into every `just check`.
const QUOTE_STEPS: usize = 10_000_000;

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// The folders whose Markdown files are the docs: the user docs, the
/// development docs, and the release notes the release text and changelog
/// are made from.
const PUBLISHED: [&str; 3] = ["docs", "docs/dev", "release-notes"];

/// The docs at the repository's root.
const ROOT_DOCS: [&str; 3] = ["README.md", "CONTRIBUTING.md", "SECURITY.md"];

fn docs() -> Vec<(String, String)> {
    let mut docs: Vec<(String, String)> = ROOT_DOCS
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                fs::read_to_string(repo_file(name)).unwrap(),
            )
        })
        .collect();
    for folder in PUBLISHED {
        let found: Vec<_> = fs::read_dir(repo_file(folder))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (
                    format!("{folder}/{name}"),
                    fs::read_to_string(&path).unwrap(),
                )
            })
            .collect();
        assert!(!found.is_empty(), "{folder} holds no docs");
        docs.extend(found);
    }
    docs.sort();
    docs
}

/// What a doc quotes: commands (any code) and output (marked), each with
/// the line it is on.
#[derive(Default)]
struct Quotes {
    code: Vec<(usize, String)>,
    output: Vec<(usize, String)>,
}

fn quotes(doc: &str) -> Quotes {
    let mut quotes = Quotes::default();
    // Inside a fenced block: whether it is tagged `output`.
    let mut fence: Option<bool> = None;
    let mut continued = String::new();
    for (index, line) in doc.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(rest.trim() == "output"),
            };
            continue;
        }
        match &fence {
            Some(true) => quotes.output.push((number, trimmed.to_owned())),
            Some(false) => {
                // A shell line ending in `\` goes on in the next one.
                continued.push_str(trimmed.trim_end_matches('\\'));
                continued.push(' ');
                if !trimmed.ends_with('\\') {
                    quotes.code.push((number, std::mem::take(&mut continued)));
                }
            }
            None => spans(line, number, &mut quotes),
        }
    }
    quotes
}

/// Inline code spans: a single backtick run is code, a double one output.
fn spans(line: &str, number: usize, quotes: &mut Quotes) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            i += 1;
            continue;
        }
        let run = chars[i..].iter().take_while(|&&c| c == '`').count();
        let start = i + run;
        let mut j = start;
        let mut end = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let close = chars[j..].iter().take_while(|&&c| c == '`').count();
                if close == run {
                    end = Some(j);
                    break;
                }
                j += close;
            } else {
                j += 1;
            }
        }
        let Some(end) = end else {
            break;
        };
        let text: String = chars[start..end].iter().collect();
        match run {
            1 => quotes.code.push((number, text)),
            2 if !text.contains('`') => quotes.output.push((number, text.trim().to_owned())),
            2 => quotes.code.push((number, text.trim().to_owned())),
            _ => {}
        }
        i = end + run;
    }
}

// ─── Commands ───────────────────────────────────────────────────────────────

/// The words of each `user-memory` command in a shell line, up to the first
/// shell operator. A command is the first word of a segment (after `|`,
/// `;`, `&&`, `(` or `!`), or the program a `docker compose … exec|run`
/// segment runs; anywhere else, as in `rm ~/.local/bin/user-memory`, the
/// name is a path, not a command.
fn commands(line: &str) -> Vec<Vec<String>> {
    let spaced = line
        .replace("&&", " ; ")
        .replace("||", " ; ")
        .replace(['|', ';', '(', ')'], " ; ");
    let mut found = Vec::new();
    for segment in spaced.split(" ; ") {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let words = match words.first() {
            Some(&"!") => &words[1..],
            _ => &words[..],
        };
        let program = |word: &str| word == "user-memory" || word.ends_with("/user-memory");
        let start = if words.first().is_some_and(|w| program(w)) {
            Some(0)
        } else if words.first() == Some(&"docker") {
            words.iter().position(|w| program(w))
        } else {
            None
        };
        let Some(start) = start else {
            continue;
        };
        let args: Vec<String> = words[start + 1..]
            .iter()
            .take_while(|w| !matches!(**w, ">" | "<" | "2>&1" | "#") && !w.starts_with('>'))
            .map(|w| (*w).to_owned())
            .collect();
        found.push(args);
    }
    found
}

/// The `--help` of each subcommand path, run once.
struct Help {
    cache: HashMap<Vec<String>, String>,
}

impl Help {
    fn get(&mut self, path: &[String]) -> &str {
        self.cache.entry(path.to_vec()).or_insert_with(|| {
            let out = Command::new(BINARY)
                .args(path)
                .arg("--help")
                .output()
                .unwrap();
            if out.status.success() {
                String::from_utf8_lossy(&out.stdout).into_owned()
            } else {
                String::new()
            }
        })
    }

    /// The subcommands a help text lists under `Commands:`.
    fn subcommands(text: &str) -> Vec<String> {
        text.lines()
            .skip_while(|line| !line.starts_with("Commands:"))
            .skip(1)
            .take_while(|line| line.starts_with("  "))
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect()
    }

    /// The flags a help text lists, long and short.
    fn flags(text: &str) -> Vec<String> {
        text.split(|c: char| c.is_whitespace() || c == ',')
            .filter(|word| word.starts_with('-') && word.len() > 1)
            .map(|word| {
                word.trim_end_matches(|c: char| !c.is_alphanumeric())
                    .to_owned()
            })
            .collect()
    }
}

/// Why a command does not run as written, if it does not.
fn check_command(help: &mut Help, args: &[String]) -> Option<String> {
    let mut path: Vec<String> = Vec::new();
    let mut rest = args;
    // Subcommands, as deep as the binary has them.
    while let Some(word) = rest.first() {
        if word.starts_with('-') {
            break;
        }
        let text = help.get(&path).to_owned();
        let subcommands = Help::subcommands(&text);
        if subcommands.is_empty() {
            break;
        }
        if !subcommands.contains(word) {
            return Some(format!(
                "user-memory {} has no subcommand {word}",
                path.join(" ")
            ));
        }
        path.push(word.clone());
        rest = &rest[1..];
    }
    let text = help.get(&path).to_owned();
    if text.is_empty() {
        return Some(format!("user-memory {} --help failed", path.join(" ")));
    }
    let flags = Help::flags(&text);
    for word in rest {
        if word == "-" || word == "--" || !word.starts_with('-') {
            continue;
        }
        let flag = word.split('=').next().unwrap();
        if !flags.iter().any(|known| known == flag) {
            return Some(format!(
                "user-memory {} does not accept {flag}",
                path.join(" ")
            ));
        }
    }
    None
}

fn command_errors() -> Vec<String> {
    let mut help = Help {
        cache: HashMap::new(),
    };
    let mut errors = Vec::new();
    let mut checked = 0;
    for (name, doc) in docs() {
        for (line, code) in quotes(&doc).code {
            for args in commands(&code) {
                if args.is_empty() {
                    continue;
                }
                checked += 1;
                if let Some(error) = check_command(&mut help, &args) {
                    errors.push(format!("{name}:{line}: {error}: {}", code.trim()));
                }
            }
        }
    }
    assert!(
        checked > 20,
        "only {checked} commands found; is the parser broken?"
    );
    errors
}

#[test]
fn every_documented_command_runs_as_written() {
    let errors = command_errors();
    assert!(errors.is_empty(), "\n{}", errors.join("\n"));
}

// ─── Output ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Text(Vec<char>),
    Hole,
}

/// The source patterns, with each text piece also kept as a string, so the
/// cheap tests before a match allocate nothing.
struct Corpus {
    patterns: Vec<Vec<Piece>>,
    /// Each pattern's text pieces, as strings.
    texts: Vec<Vec<String>>,
    /// Every text piece, one per line: a quote found in it is part of one
    /// string as written.
    joined: String,
}

fn corpus() -> Corpus {
    let patterns = source_patterns();
    let texts: Vec<Vec<String>> = patterns
        .iter()
        .map(|pattern| {
            pattern
                .iter()
                .filter_map(|piece| match piece {
                    Piece::Text(text) => Some(text.iter().collect()),
                    Piece::Hole => None,
                })
                .collect()
        })
        .collect();
    let joined = texts
        .iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    Corpus {
        patterns,
        texts,
        joined,
    }
}

/// Every string literal in src/, install.sh and uninstall.sh, one pattern
/// per line of each: format holes (`{…}`, and `$…` or `%s` in the scripts)
/// match any text, and escapes and `\` line continuations are resolved.
/// Comments are skipped: only what the code prints counts.
fn source_patterns() -> Vec<Vec<Piece>> {
    let mut files = vec![repo_file("src")];
    let mut patterns = Vec::new();
    // The installer and the uninstaller print in shell, not in src/.
    let scripts = ["install.sh", "uninstall.sh"]
        .map(|name| fs::read_to_string(repo_file(name)).unwrap())
        .join("\n");
    for literal in shell_literals(&scripts) {
        for line in literal.split('\n') {
            let pattern = holes(line);
            if substantive(&pattern) {
                patterns.push(pattern);
            }
        }
    }
    while let Some(next) = files.pop() {
        if next.is_dir() {
            files.extend(fs::read_dir(next).unwrap().map(|e| e.unwrap().path()));
        } else if next.extension().is_some_and(|ext| ext == "rs") {
            for literal in literals(&fs::read_to_string(&next).unwrap()) {
                for line in literal.split('\n') {
                    let pattern = holes(line);
                    if substantive(&pattern) {
                        patterns.push(pattern);
                    }
                }
            }
        }
    }
    patterns
}

/// The strings a shell script prints, in the format `holes` reads: quoted
/// strings and here-document lines, with `$name`, `${…}`, `$(…)` and
/// printf's `%s` as holes, shell escapes resolved and comments skipped.
fn shell_literals(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut found = Vec::new();
    // Here-documents start after the line that opens them: (delimiter,
    // whether it was quoted, so nothing in it expands).
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\n' => {
                i += 1;
                for (delimiter, quoted) in std::mem::take(&mut heredocs) {
                    while i < chars.len() {
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == '\n')
                            .map_or(chars.len(), |n| i + n);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(chars.len());
                        if line.trim() == delimiter {
                            break;
                        }
                        found.push(if quoted { braces(&line) } else { expand(&line) });
                    }
                }
            }
            '#' if i == 0 || chars[i - 1].is_whitespace() => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                let mut j = i + 2;
                if chars.get(j) == Some(&'-') {
                    j += 1;
                }
                let quoted = matches!(chars.get(j), Some('\'' | '"'));
                if quoted {
                    j += 1;
                }
                let start = j;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j > start {
                    heredocs.push((chars[start..j].iter().collect(), quoted));
                }
                i = j + usize::from(quoted);
            }
            '\'' => {
                let end = chars[i + 1..]
                    .iter()
                    .position(|&c| c == '\'')
                    .map_or(chars.len(), |n| i + 1 + n);
                let text: String = chars[i + 1..end].iter().collect();
                found.push(
                    braces(&text)
                        .replace("%s", "{}")
                        .replace("%d", "{}")
                        .replace("\\n", "\n"),
                );
                i = end + 1;
            }
            '"' => {
                let mut text = String::new();
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == '\\' && matches!(chars.get(j + 1), Some('"' | '\\' | '$' | '`'))
                    {
                        text.push(chars[j + 1]);
                        j += 2;
                    } else if chars[j] == '\\' && chars.get(j + 1) == Some(&'\n') {
                        j += 2;
                    } else {
                        text.push(chars[j]);
                        j += 1;
                    }
                }
                found.push(expand(&text));
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    found
}

/// Literal shell text with `{` and `}` doubled, so `holes` reads them as
/// braces.
fn braces(text: &str) -> String {
    text.replace('{', "{{").replace('}', "}}")
}

/// Double-quoted or here-document shell text: every expansion is a hole.
fn expand(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1)) {
            ('$', Some(&open @ ('{' | '('))) => {
                let close = if open == '{' { '}' } else { ')' };
                let mut depth = 0;
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == open {
                        depth += 1;
                    } else if chars[j] == close {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                out.push_str("{}");
                i = j + 1;
            }
            ('$', Some(c))
                if c.is_alphanumeric() || matches!(c, '_' | '*' | '@' | '$' | '#' | '?') =>
            {
                let mut j = i + 2;
                if c.is_alphabetic() || *c == '_' {
                    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                        j += 1;
                    }
                }
                out.push_str("{}");
                i = j;
            }
            ('%', Some('s' | 'd')) => {
                out.push_str("{}");
                i += 2;
            }
            ('\\', Some('n')) => {
                out.push('\n');
                i += 2;
            }
            ('{', _) => {
                out.push_str("{{");
                i += 1;
            }
            ('}', _) => {
                out.push_str("}}");
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The contents of the string literals in Rust source, unescaped.
fn literals(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            '\'' => {
                // A char literal ('x', '\n', '"'), or a lifetime ('a).
                if chars.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            'r' if matches!(chars.get(i + 1), Some('"' | '#'))
                && (i == 0 || !chars[i - 1].is_alphanumeric() && chars[i - 1] != '_') =>
            {
                let hashes = chars[i + 1..].iter().take_while(|&&c| c == '#').count();
                let open = i + 1 + hashes;
                if chars.get(open) != Some(&'"') {
                    i += 1;
                    continue;
                }
                let mut j = open + 1;
                while j < chars.len() {
                    if chars[j] == '"'
                        && chars[j + 1..]
                            .iter()
                            .take(hashes)
                            .filter(|&&c| c == '#')
                            .count()
                            == hashes
                    {
                        break;
                    }
                    j += 1;
                }
                found.push(chars[open + 1..j.min(chars.len())].iter().collect());
                i = j + 1 + hashes;
            }
            '"' => {
                let mut text = String::new();
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == '\\' {
                        j += 1;
                        match chars.get(j) {
                            Some('n') => text.push('\n'),
                            Some('t') => text.push('\t'),
                            Some('\n') => {
                                while chars.get(j + 1).is_some_and(|c| c.is_whitespace()) {
                                    j += 1;
                                }
                            }
                            Some('u') => {
                                let close = chars[j..].iter().position(|&c| c == '}').unwrap();
                                let hex: String = chars[j + 2..j + close].iter().collect();
                                text.extend(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()));
                                j += close;
                            }
                            Some(&c) => text.push(c),
                            None => {}
                        }
                    } else {
                        text.push(chars[j]);
                    }
                    j += 1;
                }
                found.push(text);
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    found
}

/// A format string as text and holes: `{…}` is a hole, `{{` and `}}` are
/// braces.
fn holes(line: &str) -> Vec<Piece> {
    let chars: Vec<char> = line.chars().collect();
    let mut pieces = Vec::new();
    let mut text = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1)) {
            ('{', Some('{')) | ('}', Some('}')) => {
                text.push(chars[i]);
                i += 2;
            }
            ('{', _) => match chars[i..].iter().position(|&c| c == '}') {
                Some(close) => {
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    pieces.push(Piece::Hole);
                    i += close + 1;
                }
                None => {
                    text.push('{');
                    i += 1;
                }
            },
            (c, _) => {
                text.push(c);
                i += 1;
            }
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    pieces
}

/// A pattern with some text besides spaces: `{} ({})` joins a key and its
/// label, while `{} {}` says nothing about what it prints.
fn substantive(pattern: &[Piece]) -> bool {
    pattern.iter().any(|piece| match piece {
        Piece::Text(text) => text.iter().any(|c| !c.is_whitespace()),
        Piece::Hole => false,
    })
}

fn has_word(text: &[char]) -> bool {
    text.split(|c| !c.is_alphabetic())
        .any(|word| word.len() >= PATTERN_WORD)
}

/// Whether a quoted fragment is printed by the code. The fragment must be
/// covered by source patterns placed end to end; only the first may start,
/// and only the last may end, partway through its pattern, where the quote
/// left the rest out. Each window must match a word of the code's text. A
/// hole stands for one word (a name, a path, a number), or for text the
/// patterns print in full, piece by piece, such as a list joined with
/// `, `: any space in a hole must come from the code, so a changed message
/// cannot hide in a hole.
struct Matcher<'a> {
    corpus: &'a Corpus,
    patterns: &'a [Vec<Piece>],
    fragment: Vec<char>,
    /// For each position, the first space at or after it (the length when
    /// none): text from `x` to `y` is one word when `y <= space_after[x]`.
    space_after: Vec<usize>,
    /// Where each text piece can start in the fragment, whole or cut off by
    /// its end: the only places a hole before that text can end.
    occurrences: HashMap<Vec<char>, Rc<Vec<usize>>>,
    /// The patterns whose every text piece occurs in the fragment: the only
    /// ones that can print a part of it in full.
    whole: Vec<usize>,
    /// The patterns that start with text: the ones that may print the end of
    /// a quote that stops partway.
    text_first: Vec<usize>,
    /// Where each pattern, matched from its start at a position, can end:
    /// one match serves every end asked about, which keeps a long quote's
    /// many hole boundaries from repeating the same work.
    runs: HashMap<(usize, usize), (usize, Rc<Reach>)>,
    /// From each start, the positions that words and text the patterns
    /// print in full, one after the other, reach: one scan answers every
    /// end asked about.
    reaches: HashMap<usize, (usize, Rc<Vec<bool>>)>,
    /// Where one segment starting at each position can end.
    segments: HashMap<usize, (usize, Rc<Vec<usize>>)>,
    /// The patterns that can start at a position, by whether they are to
    /// end the quote: `whole` ones elsewhere, `text_first` ones at its end.
    starting: HashMap<(bool, usize), Rc<Vec<usize>>>,
    /// The questions being worked out now. One asked again inside itself
    /// gets the previous pass's answer, so answers only grow from pass to
    /// pass; each answer above is kept with the pass that worked it out.
    active: HashSet<(u8, usize, usize)>,
    pass: usize,
    /// How many `run_from` matches are open at each start. A pattern that
    /// starts with a hole fills it from that same start; past two open
    /// there, a hole takes the answer known so far instead of opening
    /// another, which keeps the stack shallow, and the next pass grows it.
    open_at: HashMap<usize, usize>,
    /// Whether this pass grew any answer: matching stops at a pass that
    /// grows none.
    grew: bool,
    /// Matching work done so far: positions tried in patterns, ends tried
    /// for holes and positions a reach scan visits. The same on every
    /// machine, unlike the time it takes.
    steps: usize,
}

/// Where matching a pattern got to: the positions after its last piece,
/// and whether it ran into the fragment's end partway through.
#[derive(PartialEq)]
struct Reach {
    ends: Vec<usize>,
    cut: bool,
}

impl<'a> Matcher<'a> {
    fn new(corpus: &'a Corpus, fragment: &str) -> Self {
        let patterns = &corpus.patterns;
        let whole = (0..patterns.len())
            .filter(|&i| {
                corpus.texts[i]
                    .iter()
                    .all(|text| fragment.contains(text.as_str()))
            })
            .collect();
        let text_first = (0..patterns.len())
            .filter(|&i| matches!(patterns[i].first(), Some(Piece::Text(_))))
            .collect();
        let fragment: Vec<char> = fragment.chars().collect();
        let mut space_after = vec![fragment.len(); fragment.len() + 1];
        for i in (0..fragment.len()).rev() {
            space_after[i] = if fragment[i].is_whitespace() {
                i
            } else {
                space_after[i + 1]
            };
        }
        Self {
            corpus,
            patterns,
            fragment,
            space_after,
            occurrences: HashMap::new(),
            whole,
            text_first,
            runs: HashMap::new(),
            reaches: HashMap::new(),
            segments: HashMap::new(),
            starting: HashMap::new(),
            active: HashSet::new(),
            pass: 0,
            open_at: HashMap::new(),
            grew: false,
            steps: 0,
        }
    }

    fn covers(&mut self) -> bool {
        // Most quotes are part of one string as written: the quick case.
        let quote: String = self.fragment.iter().collect();
        if has_word(&self.fragment) && self.corpus.joined.contains(&quote) {
            return true;
        }
        // Answers only grow, so a yes is final; a no is tried again while
        // the answers it relied on still grow.
        loop {
            self.pass += 1;
            self.grew = false;
            if self.covers_once() {
                return true;
            }
            if !self.grew {
                return false;
            }
        }
    }

    fn covers_once(&mut self) -> bool {
        let len = self.fragment.len();
        // Right to left, so each start's matches find every later start's
        // already worked out: the stack stays shallow on any length.
        for x in (0..len).rev() {
            self.reach(x);
        }
        let first = self.fragment[0];
        let patterns = self.patterns;
        // Windows ending at their pattern's end, by where they end.
        let mut frontier = Vec::new();
        for pattern in patterns {
            let mut starts: Vec<(usize, usize)> = Vec::new();
            for (t, piece) in pattern.iter().enumerate() {
                match piece {
                    Piece::Text(text) => starts.extend(
                        text.iter()
                            .enumerate()
                            .filter(|&(o, &c)| (t == 0 && o == 0) || c == first)
                            .map(|(o, _)| (t, o)),
                    ),
                    // A quote may also start at a hole, its text left out.
                    Piece::Hole => starts.push((t, 0)),
                }
            }
            for (t, o) in starts {
                let reach = self.run(&pattern[t..], o, 0, true);
                if reach.cut || reach.ends.contains(&len) {
                    return true;
                }
                frontier.extend(reach.ends);
            }
        }
        let mut seen = vec![false; len + 1];
        while let Some(at) = frontier.pop() {
            if at == 0 || seen[at] {
                continue;
            }
            seen[at] = true;
            for pattern in patterns {
                if !self.could_start(pattern, at, len) {
                    continue;
                }
                let reach = self.run(pattern, 0, at, true);
                if reach.cut || reach.ends.contains(&len) {
                    return true;
                }
                frontier.extend(reach.ends);
            }
        }
        false
    }

    /// Matches `pieces` (the first from offset `o`) against the fragment
    /// from `at`, to its end at most.
    fn run(&mut self, pieces: &[Piece], o: usize, at: usize, need_word: bool) -> Reach {
        // Each position carries whether the window has matched a word of
        // the code's text yet, its own or a hole's: a window of holes and
        // spaces alone proves nothing.
        let mut positions = vec![(at, false)];
        let mut cut = false;
        for (index, piece) in pieces.iter().enumerate() {
            let mut next = Vec::new();
            self.steps += positions.len();
            for &(p, worded) in &positions {
                match piece {
                    Piece::Text(text) => {
                        let text = if index == 0 { &text[o..] } else { &text[..] };
                        let limit = self.fragment.len();
                        let room = limit - p;
                        if room < text.len() {
                            // The quote stops partway through this text, or
                            // just before it.
                            // Stopping just before the text, having
                            // matched none of it, proves nothing inside a
                            // hole; a whole quote may stop there.
                            if (p < limit || (p > at && need_word))
                                && self.fragment[p..limit] == text[..room]
                                && (!need_word || worded || has_word(&text[..room]))
                            {
                                cut = true;
                            }
                        } else if self.fragment[p..p + text.len()] == text[..] {
                            next.push((p + text.len(), worded || has_word(text)));
                        }
                    }
                    Piece::Hole => {
                        let len = self.fragment.len();
                        // A hole stands for one word, up to the next space,
                        // or for text the code prints (see `reach`), and
                        // never for the whole quote.
                        let word_end = self.space_after[p];
                        let known_only = index == 0
                            && p == at
                            && self.open_at.get(&at).is_some_and(|&open| open > 1);
                        let mut reached: Option<Rc<Vec<bool>>> = None;
                        // It ends only where the text after it can start
                        // (or start to, where the quote stops).
                        let ends: Vec<usize> = match pieces.get(index + 1) {
                            Some(Piece::Text(text)) => {
                                let at_text = self.occurrences(text);
                                let first = at_text.partition_point(|&end| end <= p);
                                at_text[first..].to_vec()
                            }
                            _ => (p + 1..=len).collect(),
                        };
                        self.steps += ends.len();
                        for end in ends {
                            let printed = if end <= word_end {
                                false
                            } else if p == 0 && end == len {
                                continue;
                            } else {
                                let reached = reached.get_or_insert_with(|| {
                                    if known_only {
                                        self.reaches.get(&p).map_or_else(
                                            || Rc::new(vec![false; len + 1]),
                                            |(_, known)| Rc::clone(known),
                                        )
                                    } else {
                                        self.reach(p)
                                    }
                                });
                                if !reached[end] {
                                    continue;
                                }
                                true
                            };
                            next.push((end, worded || printed));
                        }
                    }
                }
            }
            next.sort_unstable();
            next.dedup();
            positions = next;
            if positions.is_empty() {
                break;
            }
        }
        Reach {
            ends: positions
                .into_iter()
                .filter_map(|(p, worded)| (worded || !need_word).then_some(p))
                .collect(),
            cut,
        }
    }

    /// Whether `pattern` can match from `x`: a pattern that starts with
    /// text needs that text there (or its start, where the quote stops at
    /// `limit`). A cheap test before the full match.
    fn could_start(&self, pattern: &[Piece], x: usize, limit: usize) -> bool {
        match pattern.first() {
            Some(Piece::Text(text)) => {
                let room = (limit - x).min(text.len());
                self.fragment[x..x + room] == text[..room]
            }
            _ => true,
        }
    }

    /// Matches pattern `index` from its start at `x`, to the quote's end at
    /// most, once per pass: its ends answer every segment from `x`. The
    /// words are the outer pattern's to prove: inside a hole, `{} ({})` may
    /// join a key and its label.
    fn run_from(&mut self, index: usize, x: usize) -> Rc<Reach> {
        let key = (2, index, x);
        let known = self.runs.get(&(index, x)).cloned();
        match known {
            Some((pass, reach)) if pass == self.pass || self.active.contains(&key) => {
                return reach;
            }
            None if self.active.contains(&key) => {
                return Rc::new(Reach {
                    ends: Vec::new(),
                    cut: false,
                });
            }
            _ => {}
        }
        self.active.insert(key);
        *self.open_at.entry(x).or_default() += 1;
        let patterns = self.patterns;
        let mut reach = self.run(&patterns[index], 0, x, false);
        *self.open_at.entry(x).or_default() -= 1;
        self.active.remove(&key);
        if let Some((_, old)) = &known {
            reach.ends.extend(&old.ends);
            reach.ends.sort_unstable();
            reach.ends.dedup();
            reach.cut |= old.cut;
        }
        let reach = Rc::new(reach);
        if known.as_ref().is_none_or(|(_, old)| **old != *reach) {
            self.grew = true;
        }
        self.runs.insert((index, x), (self.pass, Rc::clone(&reach)));
        reach
    }

    /// Where `text` can start: in full, or cut off by the fragment's end.
    fn occurrences(&mut self, text: &[char]) -> Rc<Vec<usize>> {
        if let Some(known) = self.occurrences.get(text) {
            return Rc::clone(known);
        }
        let len = self.fragment.len();
        let found: Rc<Vec<usize>> = Rc::new(
            (0..=len)
                .filter(|&at| {
                    let room = (len - at).min(text.len());
                    self.fragment[at..at + room] == text[..room]
                })
                .collect(),
        );
        self.occurrences.insert(text.to_vec(), Rc::clone(&found));
        found
    }

    /// The positions that words and text the patterns print in full, one
    /// after the other, reach from `x`, left to right, so a long chain
    /// costs no stack.
    fn reach(&mut self, x: usize) -> Rc<Vec<bool>> {
        let len = self.fragment.len();
        let key = (0, x, 0);
        let known = self.reaches.get(&x).cloned();
        match known {
            Some((pass, reached)) if pass == self.pass || self.active.contains(&key) => {
                return reached;
            }
            None if self.active.contains(&key) => return Rc::new(vec![false; len + 1]),
            _ => {}
        }
        self.active.insert(key);
        let mut reached = vec![false; len + 1];
        let mut from = vec![false; len + 1];
        from[x] = true;
        for p in x..len {
            if !from[p] {
                continue;
            }
            self.steps += 1;
            for &m in self.segment_ends(p).iter() {
                reached[m] = true;
                from[m] = true;
            }
        }
        self.active.remove(&key);
        if let Some((_, old)) = &known {
            for (now, before) in reached.iter_mut().zip(old.iter()) {
                *now |= *before;
            }
        }
        let reached = Rc::new(reached);
        if known.as_ref().is_none_or(|(_, old)| **old != *reached) {
            self.grew = true;
        }
        self.reaches.insert(x, (self.pass, Rc::clone(&reached)));
        reached
    }

    /// Where one segment starting at `x` can end: a word (no space), or
    /// text one pattern prints in full. At the quote's end the text may
    /// stop partway, as the quote does, so any pattern that starts with
    /// text may print it; elsewhere only a whole one. Each pattern holds
    /// text, so the holes inside are shorter: this ends.
    fn segment_ends(&mut self, x: usize) -> Rc<Vec<usize>> {
        if let Some((pass, ends)) = self.segments.get(&x)
            && *pass == self.pass
        {
            return Rc::clone(ends);
        }
        let len = self.fragment.len();
        let mut ends: Vec<usize> = (x + 1..=self.space_after[x]).collect();
        for &index in self.starting(false, x).iter() {
            let reach = self.run_from(index, x);
            ends.extend(reach.ends.iter().copied().filter(|&m| m < len));
        }
        for &index in self.starting(true, x).iter() {
            let reach = self.run_from(index, x);
            if reach.cut || reach.ends.contains(&len) {
                ends.push(len);
                break;
            }
        }
        ends.sort_unstable();
        ends.dedup();
        let ends = Rc::new(ends);
        self.segments.insert(x, (self.pass, Rc::clone(&ends)));
        ends
    }

    /// The patterns that can start at `x`: `text_first` ones when they are
    /// to end the quote, `whole` ones otherwise.
    fn starting(&mut self, at_end: bool, x: usize) -> Rc<Vec<usize>> {
        if let Some(known) = self.starting.get(&(at_end, x)) {
            return Rc::clone(known);
        }
        let len = self.fragment.len();
        let candidates = if at_end {
            &self.text_first
        } else {
            &self.whole
        };
        let found: Rc<Vec<usize>> = Rc::new(
            candidates
                .iter()
                .copied()
                .filter(|&index| self.could_start(&self.patterns[index], x, len))
                .collect(),
        );
        self.starting.insert((at_end, x), Rc::clone(&found));
        found
    }
}

/// The parts of a quoted line the code must print: the text between `…`
/// and `<placeholder>`s.
fn fragments(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '…' => parts.push(std::mem::take(&mut current)),
            '<' if line.contains('>') => {
                parts.push(std::mem::take(&mut current));
                for next in chars.by_ref() {
                    if next == '>' {
                        break;
                    }
                }
            }
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .filter(|part| part.chars().filter(|c| c.is_alphanumeric()).count() >= FRAGMENT_CHARS)
        .collect()
}

/// A line of a quoted block that shows a memory or a group of them, not a
/// message: `  7c4e1f20 Title`, `+ 5e2a91c0 Title`, `- 0b9d3a11`,
/// `  programming (1)`.
fn data_line(line: &str) -> bool {
    let line = line.trim_start();
    let line = line
        .strip_prefix(['+', '~', '-'])
        .map_or(line, str::trim_start);
    let id = line.split_whitespace().next().unwrap_or("");
    let memory = id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit());
    let group = line.rsplit_once(" (").is_some_and(|(name, count)| {
        !name.contains(' ') && count.trim_end_matches(')').parse::<u32>().is_ok()
    });
    memory || group
}

/// The labels the session header groups projects by: the constants
/// src/session.rs imports from src/session/live.rs to build it, read from
/// the source, so a label renamed there fails the docs that quote it.
fn header_labels() -> Vec<String> {
    let header = fs::read_to_string(repo_file("src/session.rs")).unwrap();
    let live = fs::read_to_string(repo_file("src/session/live.rs")).unwrap();
    let imported = header
        .lines()
        .find_map(|line| line.trim().strip_prefix("use live::{"))
        .and_then(|rest| rest.strip_suffix("};"))
        .expect("src/session.rs imports the project labels with `use live::{…};`");
    let labels: Vec<String> = imported
        .split(',')
        .map(str::trim)
        .map(|name| {
            let start = format!("const {name}: &str = \"");
            live.lines()
                .find_map(|line| line.split_once(&start).map(|(_, value)| value))
                .and_then(|value| value.strip_suffix("\";"))
                .unwrap_or_else(|| panic!("src/session/live.rs defines no string {name}"))
                .to_owned()
        })
        .collect();
    assert!(labels.len() >= 2, "only {labels:?} found as project labels");
    labels
}

/// Why a quoted `Projects:` line uses a group label the code does not
/// define, if it does. Groups are `<keys> (<label>)` or `<label>: <keys>`,
/// separated by `; `.
fn projects_line_error(quote: &str, labels: &[String]) -> Option<String> {
    let groups = quote.strip_prefix("Projects: ")?;
    let groups = groups
        .split(". Use them as project")
        .next()
        .unwrap_or(groups);
    for group in groups.split("; ") {
        let label = match group.split_once(": ") {
            Some((label, _)) => label,
            None => group
                .rsplit_once(" (")
                .and_then(|(_, label)| label.strip_suffix(')'))
                .unwrap_or(group),
        };
        if !labels.iter().any(|known| known == label) {
            return Some(format!(
                "the Projects line groups by {label:?}, which is none of the code's labels {labels:?}"
            ));
        }
    }
    None
}

fn output_errors() -> Vec<String> {
    let corpus = corpus();
    let labels = header_labels();
    let mut errors = Vec::new();
    let mut checked = 0;
    for (name, doc) in docs() {
        for (line, quote) in quotes(&doc).output {
            if data_line(&quote) {
                continue;
            }
            if let Some(error) = projects_line_error(&quote, &labels) {
                errors.push(format!("{name}:{line}: {error}"));
            }
            for fragment in fragments(&quote) {
                checked += 1;
                let mut matcher = Matcher::new(&corpus, &fragment);
                let covered = matcher.covers();
                if matcher.steps > QUOTE_STEPS {
                    errors.push(format!(
                        "{name}:{line}: matching took {} steps, over the {QUOTE_STEPS} bound: the matcher has grown slow",
                        matcher.steps
                    ));
                }
                if !covered {
                    errors.push(format!("{name}:{line}: no code prints {fragment:?}"));
                }
            }
        }
    }
    assert!(
        checked > 5,
        "only {checked} quotes found; is the parser broken?"
    );
    errors
}

#[test]
fn every_quoted_output_is_printed_by_the_code() {
    let errors = output_errors();
    assert!(errors.is_empty(), "\n{}", errors.join("\n"));
}

// ─── The checks themselves ──────────────────────────────────────────────────

#[test]
fn a_flag_the_binary_lacks_is_caught() {
    let mut help = Help {
        cache: HashMap::new(),
    };
    let words = |line: &str| commands(line).remove(0);
    assert_eq!(
        check_command(&mut help, &words("user-memory setup --no-hooks")),
        None
    );
    assert!(check_command(&mut help, &words("user-memory setup --no-hook")).is_some());
    assert!(check_command(&mut help, &words("user-memory configure")).is_some());
    assert_eq!(
        check_command(
            &mut help,
            &words("docker compose -f x.yaml exec -T memory user-memory export - > m.json")
        ),
        None
    );
    assert!(commands("rm ~/.local/bin/user-memory").is_empty());
}

#[test]
fn a_projects_group_the_code_does_not_name_is_caught() {
    let labels = header_labels();
    for label in ["this folder", "linked", "listed in .user-memory"] {
        assert!(labels.iter().any(|known| known == label), "{labels:?}");
    }
    let line =
        |groups: &str| format!("Projects: {groups}. Use them as project for scoped memories.");
    assert_eq!(
        projects_line_error(
            &line("github.com/x/a (this folder); linked: github.com/x/b, github.com/x/c"),
            &labels
        ),
        None
    );
    assert!(
        projects_line_error(
            &line("github.com/x/a (this folder); joined: github.com/x/b"),
            &labels
        )
        .is_some()
    );
    assert!(projects_line_error(&line("github.com/x/a (this directory)"), &labels).is_some());
}

#[test]
fn a_message_the_code_does_not_print_is_caught() {
    let corpus = corpus();
    let covered = |fragment: &str| Matcher::new(&corpus, fragment).covers();
    assert!(covered("Codex session hook: OK (approved in Codex)"));
    assert!(covered("Remote MCP connection and tools: OK"));
    // A hole may hold any printed text, so only the code's own text can
    // tell a changed message: here, a misspelt word of it.
    assert!(!covered("Codex sesion hook: OK (approved in Codex)"));
    assert!(!covered("Remote MCP connection and tools: GOOD"));
    // The installer's prompts, from install.sh.
    assert!(covered("Run it in Docker?"));
    assert!(covered("(input hidden):"));
    assert!(!covered("Run it with Docker?"));
    assert!(!covered("(hidden input):"));
}

/// The longest header line the code prints: the No-project line naming as
/// many repositories as it lists, then counting the rest. It must match
/// within the step bound, so no quote of it can be too slow.
#[test]
fn the_longest_no_project_line_matches_in_time() {
    let session = fs::read_to_string(repo_file("src/session.rs")).unwrap();
    let listed: usize = session
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("const REPOSITORIES_LISTED: usize = ")
        })
        .and_then(|rest| rest.strip_suffix(';'))
        .expect("src/session.rs names how many repositories it lists")
        .parse()
        .unwrap();
    let projects = fs::read_to_string(repo_file("docs/projects.md")).unwrap();
    let example = "compiler (github.com/acme/compiler), notebooks (github.com/acme/notebooks), notes (local:notes)";
    let quote = projects
        .lines()
        .find(|line| line.contains(example))
        .expect("projects.md quotes the No-project line with its repositories");
    let mut repositories: Vec<String> = (0..listed)
        .map(|i| format!("repo{i:02} (github.com/acme/repo{i:02})"))
        .collect();
    repositories.push("and 3 more".into());
    let longest = quote.replace(example, &repositories.join(", "));
    let corpus = corpus();
    for fragment in fragments(&longest) {
        let mut matcher = Matcher::new(&corpus, &fragment);
        assert!(matcher.covers(), "{fragment}");
        assert!(
            matcher.steps <= QUOTE_STEPS,
            "{} steps for {} characters",
            matcher.steps,
            fragment.len()
        );
    }
}

/// The repository's own pages on GitHub, which release notes and the
/// installers link to by URL.
const OWN_PAGES: &str = "https://github.com/l7aromeo/user-memory/blob/main/";

/// The anchors GitHub gives a Markdown file's headings: lower case, with
/// spaces as hyphens and other punctuation dropped.
fn anchors(doc: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut fenced = false;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let Some(heading) = line.strip_prefix('#') else {
            continue;
        };
        if fenced {
            continue;
        }
        let slug: String = heading
            .trim_start_matches('#')
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
            .map(|c| if c == ' ' { '-' } else { c })
            .collect();
        found.insert(slug);
    }
    found
}

/// Each link in `doc` (named `name`, relative to the repository) whose
/// file or heading is missing: relative links, and links to this
/// repository's own pages on GitHub.
fn link_errors(name: &str, doc: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let dir = Path::new(name).parent().unwrap_or(Path::new(""));
    let mut fenced = false;
    for (index, line) in doc.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        for (at, _) in line.match_indices("](") {
            let rest = &line[at + 2..];
            let Some(target) = rest.split([')', ' ']).next() else {
                continue;
            };
            let (file, anchor) = if let Some(page) = target.strip_prefix(OWN_PAGES) {
                let (file, anchor) = page.split_once('#').unwrap_or((page, ""));
                (PathBuf::from(file), anchor)
            } else if target.contains("://") || target.starts_with("mailto:") {
                continue;
            } else {
                let (file, anchor) = target.split_once('#').unwrap_or((target, ""));
                let file = if file.is_empty() {
                    PathBuf::from(name)
                } else {
                    normalize(&dir.join(file))
                };
                (file, anchor)
            };
            let number = index + 1;
            let Ok(text) = fs::read_to_string(repo_file(&file.to_string_lossy())) else {
                if !repo_file(&file.to_string_lossy()).exists() {
                    errors.push(format!(
                        "{name}:{number}: {target}: no file {}",
                        file.display()
                    ));
                }
                continue;
            };
            if !anchor.is_empty()
                && file.extension().is_some_and(|ext| ext == "md")
                && !anchors(&text).contains(anchor)
            {
                errors.push(format!("{name}:{number}: {target}: no heading #{anchor}"));
            }
        }
    }
    errors
}

/// `path` with its `..` and `.` parts resolved, as a link names a file.
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str()),
        }
    }
    parts.iter().collect()
}

// Every link between the docs, and every link to this repository's own
// pages, reaches a file and heading that exist, so moving or renaming a doc
// or a heading cannot leave a link broken.
#[test]
fn every_link_reaches_its_file_and_heading() {
    let errors: Vec<String> = docs()
        .iter()
        .flat_map(|(name, doc)| link_errors(name, doc))
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn a_broken_link_is_caught() {
    let doc = "See [install](install.md#install-on-windows), [gone](gone.md),\n\
               [a heading](install.md#no-such-heading), [up](../README.md) and\n\
               [the page](https://github.com/l7aromeo/user-memory/blob/main/docs/missing.md).\n";
    assert_eq!(
        link_errors("docs/x.md", doc),
        [
            "docs/x.md:1: gone.md: no file docs/gone.md",
            "docs/x.md:2: install.md#no-such-heading: no heading #no-such-heading",
            "docs/x.md:3: https://github.com/l7aromeo/user-memory/blob/main/docs/missing.md: no file docs/missing.md",
        ]
    );
}
