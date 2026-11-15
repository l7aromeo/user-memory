//! The docs and release notes (README.md, CONTRIBUTING.md, SECURITY.md,
//! AGENTS.md, docs/ and release-notes/) against the code, so docs that
//! disagree with the binary cannot be released: scripts/client-release runs
//! this test before it builds.
//!
//! - Every `user-memory …` command in a code block or inline code span must
//!   name subcommands the built binary has, with flags its `--help` lists.
//! - Every quoted output must come from a string in src/, install.sh or
//!   uninstall.sh. Strings in test code don't count.
//!
//! Marking quoted output, for whoever writes the docs:
//!
//! - A fenced block tagged `output` is checked line by line.
//! - An inline span in double backticks (``like this``) is a quoted message;
//!   it renders as plain code. A double-backtick span that holds a backtick
//!   is ordinary inline code, as Markdown means it, and is not checked.
//! - In a quote, `…` and `<placeholder>` stand for text left out. A word
//!   joined to one, such as `/.git` in `…/.git`, is checked only when a
//!   string in the code holds it: the rest of a path the code builds is in
//!   none.
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

use proc_macro2::{Delimiter, TokenStream, TokenTree};

const BINARY: &str = env!("CARGO_BIN_EXE_user-memory");

/// A pattern's text must hold a word this long, so a pattern of holes and
/// punctuation such as `{}: {}` cannot match any line; `{}: OK` can.
const PATTERN_WORD: usize = 2;

/// A quoted fragment with fewer letters and digits than this, such as the
/// `(` left between two placeholders, says nothing to check.
const FRAGMENT_CHARS: usize = 3;

/// The most matching work one quote may take, in matcher steps. Counted,
/// not timed, so a slow machine fails nothing: a doc's quotes take at most
/// about 12,000 steps, and the longest No-project line the code can print
/// about 240,000. A matcher that backtracks over hole boundaries, or that
/// matches patterns which cannot change the answer, takes millions on that
/// line, so a slowdown that size fails here instead of creeping into every
/// `just check`.
const QUOTE_STEPS: usize = 10_000_000;

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// The folders whose Markdown files are the docs: the user docs, the
/// development docs, the release notes the release text and changelog are
/// made from, and the user-memory skill's playbooks, which agents follow.
const PUBLISHED: [&str; 4] = ["docs", "docs/dev", "release-notes", "skills/user-memory"];

/// The docs at the repository's root.
const ROOT_DOCS: [&str; 4] = ["README.md", "CONTRIBUTING.md", "SECURITY.md", "AGENTS.md"];

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

/// The compose service that runs user-memory, in the repository's
/// `compose.yaml`, the installer's and the docs alike. Its image's entrypoint
/// is the binary, so `docker compose run memory migrate` runs `user-memory
/// migrate`; another service, such as `ollama`, runs another program.
const SERVICE: &str = "memory";

/// In a `docker compose … run|exec …` segment, the position of the
/// [`SERVICE`] word when the words after it are user-memory's arguments: no
/// `--entrypoint` replaces the binary.
fn service(words: &[&str]) -> Option<usize> {
    let verb = words.iter().position(|w| matches!(*w, "run" | "exec"))?;
    if words.contains(&"--entrypoint") {
        return None;
    }
    let at = words[verb + 1..].iter().position(|w| *w == SERVICE)?;
    Some(verb + 1 + at)
}

/// Stands in for a `|` with a word on each side and no space, which lists
/// alternatives (`keep-store|use-file`, `--no-hooks|--hooks`); with a space
/// on either side it is a pipe.
const ALTERNATIVES: char = '\u{1}';

/// `line` with each `|` that lists alternatives written as [`ALTERNATIVES`].
fn mark_alternatives(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let word = |c: Option<&char>| c.is_some_and(|c| !c.is_whitespace() && *c != '|');
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let between = i > 0 && word(chars.get(i - 1)) && word(chars.get(i + 1));
            if c == '|' && between { ALTERNATIVES } else { c }
        })
        .collect()
}

/// The words of each `user-memory` command in a shell line, up to the first
/// shell operator. A command is the first word of a segment (after `|`,
/// `;`, `&&`, `(` or `!`), the program a `docker compose … exec|run`
/// segment runs, or the arguments it gives the [`SERVICE`] service; anywhere
/// else, as in `rm ~/.local/bin/user-memory`, the name is a path, not a
/// command.
fn commands(line: &str) -> Vec<Vec<String>> {
    let spaced = mark_alternatives(line)
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
            words
                .iter()
                .position(|w| program(w))
                .or_else(|| service(words))
        } else {
            None
        };
        let Some(start) = start else {
            continue;
        };
        let args: Vec<String> = words[start + 1..]
            .iter()
            .take_while(|w| !matches!(**w, ">" | "<" | "2>&1" | "#") && !w.starts_with('>'))
            .map(|w| w.replace(ALTERNATIVES, "|"))
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
    let words: HashSet<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .collect();
    for word in rest {
        // Optional parts are in brackets, and a repeatable one ends in `…`.
        let word = word.trim_start_matches('[').trim_end_matches([']', '…']);
        for part in word.split('|') {
            if part.starts_with('-') && part != "-" && part != "--" {
                let flag = part.split('=').next().unwrap();
                if !flags.iter().any(|known| known == flag) {
                    return Some(format!(
                        "user-memory {} does not accept {flag}",
                        path.join(" ")
                    ));
                }
            } else if word.contains('|') && !words.contains(part) {
                return Some(format!(
                    "user-memory {} takes no value {part}",
                    path.join(" ")
                ));
            }
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Piece {
    Text(Vec<char>),
    Hole,
}

/// The source patterns, with each text piece also kept as a string, so the
/// cheap tests before a match allocate nothing.
struct Corpus {
    /// Each distinct pattern once. The code prints many alike, such as
    /// `{}.{}.{}` for each version, and matching a copy again finds nothing
    /// new: kept, each copy would add its work to every quote. A pattern of
    /// holes and spaces alone is left out (see `substantive`): inside a
    /// hole, `{} {} {}` would print any three words.
    patterns: Vec<Vec<Piece>>,
    /// Each pattern's text pieces, as strings.
    texts: Vec<Vec<String>>,
    /// Every text piece, one per line: a quote found in it is part of one
    /// string as written.
    joined: String,
    /// Where in a pattern a quote may start, its earlier text left out: at
    /// any character of a text piece, or at a hole. Many patterns end
    /// alike, such as every one that ends in a hole, so each distinct rest
    /// of a pattern is kept once, and in an order of its own, whatever
    /// order the code holds the patterns in: a pattern added to the code
    /// adds only its own starts.
    starts: Vec<Start>,
    /// The starts in text, by their first character.
    text_starts: HashMap<char, Vec<usize>>,
    /// The starts at a hole, which may start at any character.
    hole_starts: Vec<usize>,
}

/// A place a quote may start: a pattern, a piece of it, and an offset into
/// that piece's text.
#[derive(Clone, Copy)]
struct Start {
    pattern: usize,
    piece: usize,
    offset: usize,
}

fn corpus() -> Corpus {
    Corpus::new(source_patterns())
}

impl Corpus {
    fn new(mut patterns: Vec<Vec<Piece>>) -> Self {
        let mut seen = HashSet::new();
        patterns.retain(|pattern| substantive(pattern) && seen.insert(pattern.clone()));
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
        // Each distinct rest of a pattern, from one of its pieces, once.
        let mut rests: HashMap<&[Piece], (usize, usize)> = HashMap::new();
        for (pattern, pieces) in patterns.iter().enumerate() {
            for piece in 0..pieces.len() {
                rests.entry(&pieces[piece..]).or_insert((pattern, piece));
            }
        }
        let mut rests: Vec<(&[Piece], (usize, usize))> = rests.into_iter().collect();
        rests.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut starts = Vec::new();
        for (rest, (pattern, piece)) in rests {
            let offsets = match &rest[0] {
                Piece::Text(text) => text.len(),
                Piece::Hole => 1,
            };
            starts.extend((0..offsets).map(|offset| Start {
                pattern,
                piece,
                offset,
            }));
        }
        let mut text_starts: HashMap<char, Vec<usize>> = HashMap::new();
        let mut hole_starts = Vec::new();
        for (index, start) in starts.iter().enumerate() {
            match &patterns[start.pattern][start.piece] {
                Piece::Text(text) => text_starts
                    .entry(text[start.offset])
                    .or_default()
                    .push(index),
                Piece::Hole => hole_starts.push(index),
            }
        }
        Self {
            patterns,
            texts,
            joined,
            starts,
            text_starts,
            hole_starts,
        }
    }
}

/// Every string literal in src/ outside test code, install.sh and
/// uninstall.sh, one pattern per line of each: format holes (`{…}`, and
/// `$…` or `%s` in the scripts) match any text, and escapes and `\` line
/// continuations are resolved. Comments are skipped: only what the code
/// prints counts.
fn source_patterns() -> Vec<Vec<Piece>> {
    let mut files = vec![repo_file("src")];
    let mut patterns = Vec::new();
    // The installer and the uninstaller print in shell, not in src/.
    let scripts = ["install.sh", "uninstall.sh"]
        .map(|name| fs::read_to_string(repo_file(name)).unwrap())
        .join("\n");
    for literal in shell_literals(&scripts) {
        patterns.extend(literal.split('\n').map(holes));
    }
    let mut sources = Vec::new();
    while let Some(next) = files.pop() {
        if next.is_dir() {
            files.extend(fs::read_dir(next).unwrap().map(|e| e.unwrap().path()));
        } else if next.extension().is_some_and(|ext| ext == "rs") {
            let source = fs::read_to_string(&next).unwrap();
            sources.push((next, source));
        }
    }
    patterns.extend(rust_patterns(&sources));
    patterns
}

/// The patterns of Rust source files, given as (path, source).
fn rust_patterns(sources: &[(PathBuf, String)]) -> Vec<Vec<Piece>> {
    let mut sources: Vec<(&PathBuf, Literals)> = sources
        .iter()
        .map(|(path, source)| (path, literals(source)))
        .collect();
    // A test-only module in a file of its own, and the modules inside it.
    let test_files: Vec<PathBuf> = sources
        .iter()
        .flat_map(|(path, found)| {
            found
                .test_modules
                .iter()
                .map(|module| module_path(path, module))
        })
        .collect();
    sources.retain(|(path, _)| {
        !test_files
            .iter()
            .any(|test| **path == test.with_extension("rs") || path.starts_with(test))
    });
    // A name two constants share, as one per platform may, stays a hole.
    let mut constants: HashMap<String, Option<String>> = HashMap::new();
    for (name, text) in sources.iter().flat_map(|(_, found)| &found.constants) {
        constants
            .entry(name.clone())
            .and_modify(|known| {
                if known.as_ref() != Some(text) {
                    *known = None;
                }
            })
            .or_insert_with(|| Some(text.clone()));
    }
    let mut patterns = Vec::new();
    for (_, found) in sources {
        for literal in found.strings {
            patterns.extend(with_constants(&literal, &constants).split('\n').map(holes));
        }
    }
    patterns
}

/// A format string with each hole that names a string constant, such as
/// `{NO_PROJECT}`, replaced by the constant's text, so a line the code joins
/// from constants is matched against their text, not against any text.
fn with_constants(format: &str, constants: &HashMap<String, Option<String>>) -> String {
    let mut out = String::new();
    let mut rest = format;
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with("{{") || rest.starts_with("}}") {
            out.push_str(&rest[..2]);
            rest = &rest[2..];
            continue;
        }
        let close = if rest.starts_with('{') {
            rest.find('}')
        } else {
            None
        };
        let Some(close) = close else {
            out.push_str(&rest[..1]);
            rest = &rest[1..];
            continue;
        };
        match constants.get(&rest[1..close]) {
            Some(Some(text)) => out.push_str(&braces(text)),
            _ => out.push_str(&rest[..=close]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Where the module `a::b` declared in `file` lives, without the `.rs`: its
/// own folder, which also holds its own modules.
fn module_path(file: &Path, module: &str) -> PathBuf {
    let folder = file.parent().unwrap();
    let stem = file.file_stem().unwrap();
    let mut path = if stem == "main" || stem == "lib" {
        folder.to_path_buf()
    } else {
        folder.join(stem)
    };
    path.extend(module.split("::"));
    path
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

/// The strings in Rust source outside test code. Comments and doc comments
/// are left out: only what the code prints counts. Test code is what
/// `#[cfg(test)]` or `#[cfg(all(test, …))]` gates: an item, a statement, a
/// field or an arm, or the whole file under `#![cfg(test)]`. A test's
/// strings, such as the `{} in {}` an assertion prints, would otherwise
/// fill a hole with text the binary never prints.
fn literals(source: &str) -> Literals {
    let tokens: TokenStream = source
        .parse()
        .unwrap_or_else(|error| panic!("not Rust source: {error}"));
    let mut found = Literals::default();
    found.collect(tokens, &[]);
    found
}

#[derive(Default)]
struct Literals {
    /// The strings, unescaped.
    strings: Vec<String>,
    /// The test-only modules declared in files of their own
    /// (`#[cfg(test)] mod tests;`), as `a::b`.
    test_modules: Vec<String>,
    /// The string constants, `const NAME: &str = "…";`, by name.
    constants: Vec<(String, String)>,
}

/// What an attribute is to the corpus.
enum Attribute {
    /// `#[doc = "…"]`, which a `///` comment becomes: not printed.
    Doc,
    /// Gates test code: `#[cfg(test)]` before what it gates, or
    /// `#![cfg(test)]` inside it.
    Test { inner: bool },
    /// Any other, such as `#[error("…")]`, whose strings the code prints.
    Other,
}

impl Literals {
    /// Collects the strings in `tokens`, which lie inside the inline module
    /// `module`.
    fn collect(&mut self, tokens: TokenStream, module: &[String]) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        let mut i = 0;
        while i < tokens.len() {
            if let Some((after, attribute)) = attribute(&tokens, i) {
                match attribute {
                    Attribute::Test { inner: true } => return,
                    Attribute::Test { inner: false } => {
                        let end = test_item_end(&tokens, after);
                        if let Some(name) = module_file(&tokens[after..end]) {
                            self.test_modules
                                .push([module, std::slice::from_ref(&name)].concat().join("::"));
                        }
                        i = end;
                        continue;
                    }
                    Attribute::Doc => {
                        i = after;
                        continue;
                    }
                    Attribute::Other => {}
                }
            }
            match &tokens[i] {
                TokenTree::Group(group) => {
                    let inline_module = match tokens.get(i.wrapping_sub(2)..i) {
                        Some([TokenTree::Ident(word), TokenTree::Ident(name)])
                            if word == "mod" && group.delimiter() == Delimiter::Brace =>
                        {
                            Some(name.to_string())
                        }
                        _ => None,
                    };
                    match inline_module {
                        Some(name) => self.collect(
                            group.stream(),
                            &[module, std::slice::from_ref(&name)].concat(),
                        ),
                        None => self.collect(group.stream(), module),
                    }
                }
                TokenTree::Ident(word) if word == "const" => {
                    if let Some(constant) = string_constant(&tokens[i..]) {
                        self.constants.push(constant);
                    }
                }
                TokenTree::Literal(literal) => match syn::Lit::new(literal.clone()) {
                    syn::Lit::Str(text) => self.strings.push(text.value()),
                    syn::Lit::ByteStr(bytes) => self
                        .strings
                        .push(String::from_utf8_lossy(&bytes.value()).into_owned()),
                    _ => {}
                },
                TokenTree::Ident(_) | TokenTree::Punct(_) => {}
            }
            i += 1;
        }
    }
}

/// The name and text of the string constant `const NAME: &str = "…";` that
/// `tokens` start with, if they do.
fn string_constant(tokens: &[TokenTree]) -> Option<(String, String)> {
    let shape: String = tokens
        .iter()
        .take(10)
        .map(|token| match token {
            TokenTree::Ident(word) if word == "str" || word == "static" => word.to_string(),
            TokenTree::Ident(_) => "name".to_owned(),
            TokenTree::Punct(punct) => punct.as_char().to_string(),
            TokenTree::Literal(_) => "value".to_owned(),
            TokenTree::Group(_) => "group".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let value = if shape.starts_with("name name : & str = value ;") {
        &tokens[6]
    } else if shape.starts_with("name name : & ' static str = value ;") {
        &tokens[8]
    } else {
        return None;
    };
    let (TokenTree::Ident(name), TokenTree::Literal(value)) = (&tokens[1], value) else {
        return None;
    };
    match syn::Lit::new(value.clone()) {
        syn::Lit::Str(value) => Some((name.to_string(), value.value())),
        _ => None,
    }
}

/// The attribute at `i`, if one starts there (`#[…]` or `#![…]`), with the
/// index after it.
fn attribute(tokens: &[TokenTree], i: usize) -> Option<(usize, Attribute)> {
    let TokenTree::Punct(hash) = tokens.get(i)? else {
        return None;
    };
    if hash.as_char() != '#' {
        return None;
    }
    let inner = matches!(tokens.get(i + 1), Some(TokenTree::Punct(bang)) if bang.as_char() == '!');
    let at = i + 1 + usize::from(inner);
    let TokenTree::Group(group) = tokens.get(at)? else {
        return None;
    };
    if group.delimiter() != Delimiter::Bracket {
        return None;
    }
    let body: Vec<TokenTree> = group.stream().into_iter().collect();
    let attribute = match body.as_slice() {
        [TokenTree::Ident(doc), ..] if doc == "doc" => Attribute::Doc,
        [TokenTree::Ident(cfg), TokenTree::Group(predicate)]
            if cfg == "cfg" && requires_test(predicate.stream()) =>
        {
            Attribute::Test { inner }
        }
        _ => Attribute::Other,
    };
    Some((at + 1, attribute))
}

/// Whether a `cfg` predicate holds only in a test build: `test`, or `all`
/// with `test` among its terms.
fn requires_test(predicate: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = predicate.into_iter().collect();
    match tokens.as_slice() {
        [TokenTree::Ident(test)] => test == "test",
        [TokenTree::Ident(all), TokenTree::Group(terms)] if all == "all" => terms
            .stream()
            .into_iter()
            .collect::<Vec<_>>()
            .split(|token| matches!(token, TokenTree::Punct(comma) if comma.as_char() == ','))
            .any(|term| requires_test(term.iter().cloned().collect())),
        _ => false,
    }
}

/// Words that may come before the keyword of an item.
const QUALIFIERS: [&str; 6] = ["pub", "unsafe", "async", "extern", "default", "const"];

/// Items that end with their body (or a `;` when they have none). The
/// commas in their generics end nothing.
const ITEMS: [&str; 8] = [
    "fn",
    "mod",
    "struct",
    "enum",
    "union",
    "impl",
    "trait",
    "macro_rules",
];

/// Statements that end only with `;`, whatever commas or braces their type
/// or value holds.
const DECLARATIONS: [&str; 6] = ["let", "const", "static", "type", "use", "crate"];

/// Expressions whose block ends them, so no `;` need follow; so does a
/// bare `{ … }` or `unsafe { … }`.
const BLOCKS: [&str; 5] = ["if", "match", "loop", "while", "for"];

/// Where the item, statement, field or arm that starts at `i` ends: after
/// its own further attributes, at the `;` or `,` that ends it, or after the
/// block that does. Braces, brackets and parentheses are groups of their
/// own, so only what lies at this level can end it.
fn test_item_end(tokens: &[TokenTree], mut i: usize) -> usize {
    while let Some((after, _)) = attribute(tokens, i) {
        i = after;
    }
    let word = |at: usize| match tokens.get(at) {
        Some(TokenTree::Ident(word)) => Some(word.to_string()),
        _ => None,
    };
    let punct = |at: usize| match tokens.get(at) {
        Some(TokenTree::Punct(punct)) => Some(punct.as_char()),
        _ => None,
    };
    let mut first = i;
    // `pub(crate)`, `extern "C"`, `const fn`: what comes before the keyword.
    loop {
        match (word(first).as_deref(), tokens.get(first + 1)) {
            (Some("const"), Some(TokenTree::Ident(next)))
                if !QUALIFIERS.iter().chain(&ITEMS).any(|item| next == item) =>
            {
                break;
            }
            (Some(qualifier), _) if QUALIFIERS.contains(&qualifier) => first += 1,
            (None, _)
                if first > i
                    && matches!(tokens.get(first), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis) =>
            {
                first += 1;
            }
            (None, _) if first > i && matches!(tokens.get(first), Some(TokenTree::Literal(_))) => {
                first += 1;
            }
            _ => break,
        }
    }
    let lead = word(first);
    let lead = lead.as_deref();
    let item = lead.is_some_and(|lead| ITEMS.contains(&lead));
    let declaration = lead.is_some_and(|lead| DECLARATIONS.contains(&lead));
    let block = lead.is_some_and(|lead| BLOCKS.contains(&lead))
        || matches!(tokens.get(first), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace);
    let mut at = first;
    while at < tokens.len() {
        match &tokens[at] {
            TokenTree::Punct(end) if end.as_char() == ';' => return at + 1,
            TokenTree::Punct(end) if end.as_char() == ',' && !item && !declaration => {
                return at;
            }
            TokenTree::Group(body) if body.delimiter() == Delimiter::Brace && !declaration => {
                // A macro called with braces, `name! { … }`, and an arm's
                // block, `… => { … }`, end with it too.
                let macro_call = punct(at.wrapping_sub(1)) == Some('!');
                let arm = punct(at.wrapping_sub(1)) == Some('>')
                    && punct(at.wrapping_sub(2)) == Some('=');
                if item || block || macro_call || arm {
                    if word(at + 1).as_deref() == Some("else") {
                        at += 2;
                        continue;
                    }
                    return at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// The module a test-only `mod name;` puts in a file of its own.
fn module_file(item: &[TokenTree]) -> Option<String> {
    let words: Vec<String> = item
        .iter()
        .filter_map(|token| match token {
            TokenTree::Ident(word) => Some(word.to_string()),
            _ => None,
        })
        .collect();
    let ends_bare = matches!(item.last(), Some(TokenTree::Punct(end)) if end.as_char() == ';');
    match words.as_slice() {
        [.., keyword, name] if keyword == "mod" && ends_bare => Some(name.clone()),
        _ => None,
    }
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
        Piece::Text(text) => has_text(text),
        Piece::Hole => false,
    })
}

/// Whether a pattern's text holds a space, so that inside a hole it can
/// print text a chain of words cannot reach. Text without one lies within a
/// word, which a word segment reaches already, and its holes reach only what
/// the chain does: such a pattern, `{}.{}.{}` or `{}/{}`, adds no end to a
/// segment, only work.
fn crosses_space(pattern: &[Piece]) -> bool {
    pattern.iter().any(|piece| match piece {
        Piece::Text(text) => text.iter().any(|c| c.is_whitespace()),
        Piece::Hole => false,
    })
}

fn has_word(text: &[char]) -> bool {
    text.split(|c| !c.is_alphabetic())
        .any(|word| word.len() >= PATTERN_WORD)
}

/// Whether text holds anything besides spaces.
fn has_text(text: &[char]) -> bool {
    text.iter().any(|c| !c.is_whitespace())
}

/// Whether every bracket in `text`, round, square or curly, is closed in
/// order, and none closed that it did not open.
fn balanced(text: &[char]) -> bool {
    let mut open = Vec::new();
    for &c in text {
        match c {
            '(' => open.push(')'),
            '[' => open.push(']'),
            '{' => open.push('}'),
            ')' | ']' | '}' if open.pop() != Some(c) => return false,
            _ => {}
        }
    }
    open.is_empty()
}

/// Whether a quoted fragment is printed by the code. The fragment must be
/// covered by source patterns placed end to end; only the first may start,
/// and only the last may end, partway through its pattern, where the quote
/// left the rest out. Each window must match text of its own pattern
/// besides spaces, and a word of the code's text, its own or a hole's.
///
/// A hole stands for one word (a name, a path, a number), or for text the
/// patterns print in full, one after the other, such as a list joined with
/// `, `: any space in a hole comes from the code. Unless the quote stops
/// inside it, what a hole holds closes every bracket it opens. A value that
/// is itself an unpaired bracket, such as `:(` or a regular expression in
/// an example, is refused there; no doc quotes one today.
///
/// So a message changed in the code fails the quotes of it. What this
/// cannot tell: a hole that holds a word and then text another message
/// starts with. With a note `approved in Codex` gone from the code,
/// `{} session hook: OK - {}` still covers `… OK - approved in Codex`,
/// through `approved` and the ` in {}` of other messages; in brackets, as
/// the code prints its notes, the same quote fails.
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
    /// ones that can print a part of it in full. Only those that cross a
    /// space count (see `crosses_space`).
    whole: Vec<usize>,
    /// The patterns that start with text: the ones that may print the end of
    /// a quote that stops partway. Only those that cross a space count.
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
                crosses_space(&patterns[i])
                    && corpus.texts[i]
                        .iter()
                        .all(|text| fragment.contains(text.as_str()))
            })
            .collect();
        let text_first = (0..patterns.len())
            .filter(|&i| {
                crosses_space(&patterns[i]) && matches!(patterns[i].first(), Some(Piece::Text(_)))
            })
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
        let corpus = self.corpus;
        let patterns = self.patterns;
        // Windows ending at their pattern's end, by where they end.
        let mut frontier = Vec::new();
        // Text that starts with another character cannot start the quote.
        let mut tried: Vec<usize> = corpus
            .text_starts
            .get(&self.fragment[0])
            .into_iter()
            .flatten()
            .chain(&corpus.hole_starts)
            .copied()
            .collect();
        tried.sort_unstable();
        for index in tried {
            let start = corpus.starts[index];
            let rest = &patterns[start.pattern][start.piece..];
            let reach = self.run(rest, start.offset, 0, true);
            if reach.cut || reach.ends.contains(&len) {
                return true;
            }
            frontier.extend(reach.ends);
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
        // the code's text yet, its own or a hole's, and whether it has
        // matched text of its own pattern besides spaces. A window needs
        // both: one of holes and spaces alone proves nothing, and a hole's
        // text is only as good as the pattern around it. A quote that starts
        // at a hole of `  rm -f {} {} {}` would otherwise be three words of
        // anything.
        let mut positions = vec![(at, false, false)];
        let mut cut = false;
        for (index, piece) in pieces.iter().enumerate() {
            let mut next = Vec::new();
            self.steps += positions.len();
            for &(p, worded, own) in &positions {
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
                                && (!need_word
                                    || (worded || has_word(&text[..room]))
                                        && (own || has_text(&text[..room])))
                            {
                                cut = true;
                            }
                        } else if self.fragment[p..p + text.len()] == text[..] {
                            next.push((
                                p + text.len(),
                                worded || has_word(text),
                                own || has_text(text),
                            ));
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
                            // What a hole holds closes every bracket it
                            // opens, as a name, a path or a message the
                            // code prints does, unless the quote stops
                            // inside it. Otherwise a hole could swallow the
                            // start of a bracketed note, `(approved`, and
                            // leave its end to whatever other message ends
                            // alike.
                            if end < len && !balanced(&self.fragment[p..end]) {
                                continue;
                            }
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
                            next.push((end, worded || printed, own));
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
                .filter_map(|(p, worded, own)| (worded && own || !need_word).then_some(p))
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

/// A part of a quoted line the code must print.
struct Fragment {
    text: String,
    /// Whether the part is one word joined to text left out, such as
    /// `/.git` in `…/.git`: likely the rest of a value, a path or a file
    /// name the code builds from parts rather than prints as written.
    value: bool,
}

/// The parts of a quoted line the code must print: the text between `…`
/// and `<placeholder>`s.
fn fragments(line: &str) -> Vec<Fragment> {
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
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let text = part.trim();
            let joined_before = index > 0 && !part.starts_with(char::is_whitespace);
            let joined_after = index < last && !part.ends_with(char::is_whitespace);
            Fragment {
                text: text.to_owned(),
                value: (joined_before || joined_after) && !text.contains(char::is_whitespace),
            }
        })
        .filter(|part| part.text.chars().filter(|c| c.is_alphanumeric()).count() >= FRAGMENT_CHARS)
        .collect()
}

/// A line of a quoted block that shows a memory or a group of them, not a
/// message: `  e0000002 Title`, `+ e0000006 Title`, `- e0000004 Title`,
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
            for Fragment {
                text: fragment,
                value,
            } in fragments(&quote)
            {
                checked += 1;
                let mut matcher = Matcher::new(&corpus, &fragment);
                let covered = matcher.covers();
                if matcher.steps > QUOTE_STEPS {
                    errors.push(format!(
                        "{name}:{line}: matching took {} steps, over the {QUOTE_STEPS} bound: the matcher has grown slow",
                        matcher.steps
                    ));
                }
                // A value the code builds from parts is not in its strings,
                // so only one a string holds part of can be checked.
                let built = value && !corpus.joined.contains(&fragment);
                if !covered && !built {
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
fn a_value_or_optional_flag_the_binary_lacks_is_caught() {
    let mut help = Help {
        cache: HashMap::new(),
    };
    let mut check = |line: &str| {
        let found = commands(line);
        assert_eq!(found.len(), 1, "{line}: {found:?}");
        check_command(&mut help, &found[0])
    };
    // `a|b` inside a word lists the values a flag takes; optional parts
    // are in brackets.
    assert_eq!(
        check("user-memory import f.json --apply [--on-conflict abort|keep-store|use-file]"),
        None
    );
    assert_eq!(
        check("user-memory review [--project P]… [--max-groups N]"),
        None
    );
    assert!(check("user-memory import f.json --apply --on-conflict keep-stor|use-file").is_some());
    assert!(check("user-memory review [--project P]… [--max-group N]").is_some());
    assert!(check("user-memory setup --no-hooks|--hook").is_some());
    // With spaces, `|` is a pipe.
    assert_eq!(commands("user-memory export - | gzip"), [["export", "-"]]);
}

#[test]
fn a_command_the_memory_service_runs_is_checked() {
    // The image's entrypoint is the binary, so these run user-memory.
    assert_eq!(
        commands("docker compose run --rm memory migrate --check && docker compose stop memory"),
        [["migrate", "--check"]]
    );
    assert_eq!(
        commands("docker compose -f x.yaml run --rm -T memory migrate --titles - < titles.json"),
        [["migrate", "--titles", "-"]]
    );
    // Another service, another entrypoint, or no run or exec: not user-memory.
    for line in [
        "docker compose run --rm ollama pull qwen3-embedding:4b",
        "docker compose run --rm --entrypoint rm memory -f /data/memories.sqlite3-wal",
        "docker compose up -d memory",
        "docker compose logs memory",
        "docker compose cp memory:/data/exports/b.json .",
    ] {
        assert!(commands(line).is_empty(), "{line}: {:?}", commands(line));
    }
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

/// The patterns with `from` replaced by `to` in their text.
fn replaced(patterns: &[Vec<Piece>], from: &str, to: &str) -> Vec<Vec<Piece>> {
    patterns
        .iter()
        .map(|pattern| {
            pattern
                .iter()
                .map(|piece| match piece {
                    Piece::Text(text) => Piece::Text(
                        text.iter()
                            .collect::<String>()
                            .replace(from, to)
                            .chars()
                            .collect(),
                    ),
                    Piece::Hole => Piece::Hole,
                })
                .collect()
        })
        .collect()
}

#[test]
fn a_value_changed_in_the_code_fails_the_quote_that_shows_it() {
    // The code prints the note `approved in Codex` into the hole of
    // `{} session hook: OK ({note})`. Its words are in other messages too,
    // which must not stand in for it once the code says something else.
    let quote = "Codex session hook: OK (approved in Codex)";
    let patterns = source_patterns();
    assert!(Matcher::new(&Corpus::new(patterns.clone()), quote).covers());
    let changed = Corpus::new(replaced(&patterns, "approved in Codex", "trusted"));
    assert!(!Matcher::new(&changed, quote).covers());
}

#[test]
fn a_pattern_of_holes_and_spaces_covers_no_quote() {
    // A quote may start at a hole, the pattern's earlier text left out, so
    // what is left of `  rm -f {} {} {}` is holes and spaces. The last
    // probe has text of its own, but past where a quote of it would stop.
    let misspelt = "Codex sesion hook: OK (approved in Codex)";
    let patterns = source_patterns();
    assert!(!Matcher::new(&Corpus::new(patterns.clone()), misspelt).covers());
    for probe in [
        "{} {} {}",
        "  rm -f {} {} {}",
        "{}: {}",
        "  rm -f {} {} {} now",
    ] {
        let corpus = Corpus::new(patterns.iter().cloned().chain([holes(probe)]).collect());
        assert!(!Matcher::new(&corpus, misspelt).covers(), "{probe:?}");
    }
}

#[test]
fn a_list_the_code_joins_is_matched() {
    let corpus = corpus();
    assert!(
        Matcher::new(
            &corpus,
            "Removed: the global git ignore lines # user-memory, .user-memory, .user-memory-key (~/.config/git/ignore)"
        )
        .covers()
    );
}

#[test]
fn a_hole_closes_the_brackets_it_opens() {
    let corpus = Corpus::new(
        [
            "Codex session hook: OK (approved",
            "{} in Codex); remove it",
            "Codex session hook: OK (checked)",
            "{} in Codex, then",
            "Status: {}",
        ]
        .map(holes)
        .to_vec(),
    );
    let covered = |quote: &str| Matcher::new(&corpus, quote).covers();
    // The hole of `{} in Codex); remove it` would hold the first message,
    // and leave its bracket for this one to close.
    assert!(!covered("Codex session hook: OK (approved in Codex)"));
    assert!(covered("Codex session hook: OK (checked) in Codex, then"));
    // A quote may stop inside a hole, its brackets open.
    assert!(covered("Status: Codex session hook: OK (approved"));
}

#[test]
fn strings_in_test_code_are_not_the_code_s_output() {
    let source = r##"
const SHOWN: &'static str = "shown constant";
#[cfg(test)]
const HIDDEN: &str = "hidden constant";
fn shown(x: u8) {
    #[cfg(test)]
    record(|state| {
        state.push("hidden closure");
    });
    #[cfg(all(test, unix))]
    let probe: HashMap<u8, u8> = probe("hidden let");
    #[cfg(test)]
    if x == 1 { "hidden if" } else { "hidden else" }
    println!("shown statement");
    match x {
        #[cfg(test)]
        1 => { "hidden arm" }
        _ => "shown arm",
    }
}
/// "doc comment"
struct Shown {
    #[cfg(test)]
    hidden: Field<A, B>,
    shown: u8,
}
#[cfg(test)]
thread_local! { static HIDDEN: u8 = probe("hidden macro"); }
#[cfg(test)]
#[derive(Default)]
struct Hidden<'a> { text: &'a str }
#[cfg(test)]
impl Hidden { fn new() -> Self { Self { text: "hidden impl" } } }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert_eq!(1, 1, "hidden test"); }
}
#[cfg(test)]
pub(crate) mod more_tests;
#[derive(Debug, thiserror::Error)]
#[error("shown attribute")]
struct E;
#[cfg(not(test))]
fn outside_tests() -> &'static [u8] { b"shown when not testing" }
"##;
    let found = literals(source);
    assert_eq!(
        found.strings,
        [
            "shown constant",
            "shown statement",
            "shown arm",
            "shown attribute",
            "shown when not testing"
        ]
    );
    assert_eq!(found.test_modules, ["more_tests"]);
    assert_eq!(
        found.constants,
        [("SHOWN".to_owned(), "shown constant".to_owned())]
    );
    let whole_file = literals("#![cfg(test)]\nfn f() { println!(\"hidden file\"); }");
    assert!(whole_file.strings.is_empty());
    // A test-only module in a file of its own, and the modules inside it.
    let text = |patterns: Vec<Vec<Piece>>| -> Vec<String> {
        patterns
            .iter()
            .flatten()
            .filter_map(|piece| match piece {
                Piece::Text(text) => Some(text.iter().collect()),
                Piece::Hole => None,
            })
            .collect()
    };
    let sources = [
        (
            "src/setup.rs",
            "#[cfg(test)]\nmod tests;\nfn f() { println!(\"shown setup\"); }",
        ),
        (
            "src/setup/tests.rs",
            "mod deeper; const X: &str = \"hidden tests file\";",
        ),
        (
            "src/setup/tests/deeper.rs",
            "const X: &str = \"hidden deeper file\";",
        ),
        (
            "src/main.rs",
            "#[cfg(test)]\nmod checks;\nmod inline { #[cfg(test)] mod nested; }",
        ),
        ("src/checks.rs", "const X: &str = \"hidden checks file\";"),
        (
            "src/inline/nested.rs",
            "const X: &str = \"hidden nested file\";",
        ),
        ("src/inline.rs", "const X: &str = \"shown inline file\";"),
    ]
    .map(|(path, source)| (PathBuf::from(path), source.to_owned()));
    assert_eq!(
        text(rust_patterns(&sources)),
        ["shown setup", "shown inline file"]
    );
}

#[test]
fn a_line_joined_from_constants_is_matched_against_their_text() {
    let sources = [
        (
            "src/a.rs",
            r#"const SAVED: &str = "Saved the rules."; const TWICE: &str = "Kept here.";
               fn f(x: &str) -> String { format!("{SAVED} {x} {{SAVED}} {SAVED:?}") }
               fn g(x: &str) -> String { format!("{TWICE} {x}") }"#,
        ),
        ("src/b.rs", r#"const TWICE: &str = "Kept there.";"#),
    ]
    .map(|(path, source)| (PathBuf::from(path), source.to_owned()));
    let patterns = rust_patterns(&sources);
    assert!(patterns.contains(&holes("Saved the rules. {} {{SAVED}} {}")));
    // A name two files give different text stays a hole.
    assert!(patterns.contains(&holes("{} {}")));
    let corpus = corpus();
    // The No-project line joins constants: `{NO_PROJECT} {found}`.
    assert!(
        Matcher::new(
            &corpus,
            "No project: this folder has no git remote and no .user-memory-key. If it holds several repositories"
        )
        .covers()
    );
}

/// What the matcher cannot tell apart. A hole may hold a word followed by
/// text another message starts with: here `approved`, then ` in Codex`,
/// from messages such as `{} in {}`. So a note changed in the code still
/// matches when the words the docs show are spread over other messages that
/// way. When the matcher learns to refuse this, the Matcher's comment says
/// so and this test turns around.
#[test]
fn a_note_made_of_other_messages_words_is_a_known_limit() {
    let patterns: Vec<Vec<Piece>> = source_patterns()
        .into_iter()
        .chain([holes("{} session hook: OK - {}")])
        .collect();
    let corpus = Corpus::new(replaced(&patterns, "approved in Codex", "trusted"));
    assert!(Matcher::new(&corpus, "Codex session hook: OK - approved in Codex").covers());
    assert!(!Matcher::new(&corpus, "Codex session hook: OK - approved by Codex").covers());
}

/// The longest header line the code prints: the No-project line naming as
/// many repositories as it lists, then counting the rest.
fn longest_no_project_line() -> String {
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
    quote.replace(example, &repositories.join(", "))
}

/// The longest header line must match within the step bound, so no quote
/// of it can be too slow.
#[test]
fn the_longest_no_project_line_matches_in_time() {
    let longest = longest_no_project_line();
    let corpus = corpus();
    for Fragment { text: fragment, .. } in fragments(&longest) {
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

/// Matching work follows the patterns that can print part of a quote, not
/// how many patterns the code holds. The longest line is matched again with
/// the code's patterns all repeated, with new patterns of holes and
/// punctuation, the kind that matches inside any word, and with as many
/// again that share no text with it. Only the last may add work: one step,
/// each pass, for each place one could start, its first text or a hole, to
/// rule it out. That bounds the search for a window at the quote's start,
/// which covers this line. A quote that needs windows after it as well pays
/// a few steps for each pattern that starts with a hole, at each place a
/// window ends; this test does not bound that.
#[test]
fn matching_work_does_not_grow_with_the_code_s_patterns() {
    let longest = longest_no_project_line();
    let fragment = fragments(&longest)
        .into_iter()
        .map(|fragment| fragment.text)
        .max_by_key(String::len)
        .expect("the longest line has a fragment");
    let patterns = source_patterns();
    // The same patterns with every character moved into a private use
    // plane, which no doc quotes.
    let unrelated = Corpus::new(
        patterns
            .iter()
            .map(|pattern| {
                pattern
                    .iter()
                    .map(|piece| match piece {
                        Piece::Text(text) => Piece::Text(
                            text.iter()
                                .map(|&c| {
                                    char::from_u32(0xF_0000 | (u32::from(c) & 0xFFFF))
                                        .expect("plane 15 holds every such value")
                                })
                                .collect(),
                        ),
                        Piece::Hole => Piece::Hole,
                    })
                    .collect()
            })
            .collect(),
    )
    .patterns;
    let starts: usize = unrelated
        .iter()
        .map(|pattern| {
            pattern
                .iter()
                .enumerate()
                .filter(|&(t, piece)| t == 0 || *piece == Piece::Hole)
                .count()
        })
        .sum();
    let punctuation = [
        "{}:{}",
        "{}|{}",
        "{}{}{}",
        "{}-{}.{}",
        ".{}{}{}{}",
        "{}/{}/{}",
    ]
    .map(holes);
    let steps = |corpus: &Corpus| {
        let mut matcher = Matcher::new(corpus, &fragment);
        assert!(matcher.covers(), "{fragment}");
        (matcher.steps, matcher.pass)
    };
    let (alone, _) = steps(&Corpus::new(patterns.clone()));
    // The unrelated ones first. The corpus orders the starts itself, so
    // this matters only to a start order that follows the code's.
    let (crowded, passes) = steps(&Corpus::new(
        unrelated
            .into_iter()
            .chain(patterns.iter().cloned())
            .chain(patterns.iter().cloned())
            .chain(punctuation)
            .collect(),
    ));
    assert!(
        crowded <= alone + passes * starts,
        "{crowded} steps with the added patterns, {alone} without: more than one \
         step for each of the {starts} places an unrelated pattern could start, \
         in each of {passes} passes"
    );
}

/// The repository's own pages on GitHub, which release notes and the
/// installers link to by URL.
const OWN_PAGES: &str = "https://github.com/l7aromeo/user-memory/blob/main/";

/// The anchors GitHub gives a Markdown file's headings: lower case, with
/// spaces as hyphens and other punctuation dropped, and `-1`, `-2` and so on
/// after the second and later headings of one name.
fn anchors(doc: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
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
        let earlier = seen.entry(slug.clone()).or_insert(0);
        found.insert(if *earlier == 0 {
            slug
        } else {
            format!("{slug}-{earlier}")
        });
        *earlier += 1;
    }
    found
}

/// A link from one doc into this repository.
struct Link<'a> {
    /// The line it is on, from 1.
    line: usize,
    /// As written.
    target: &'a str,
    /// The file it names, from the repository's root.
    file: String,
    /// The heading it names, or empty.
    anchor: &'a str,
}

/// The links in `doc` (named `name`, relative to the repository) to files
/// of this repository: relative links, and links to its own pages on GitHub.
fn links<'a>(name: &str, doc: &'a str) -> Vec<Link<'a>> {
    let mut found = Vec::new();
    let dir = name.rsplit_once('/').map_or("", |(dir, _)| dir);
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
                (normalize(file), anchor)
            } else if target.contains("://") || target.starts_with("mailto:") {
                continue;
            } else {
                let (file, anchor) = target.split_once('#').unwrap_or((target, ""));
                let file = if file.is_empty() {
                    name.to_owned()
                } else {
                    normalize(&format!("{dir}/{file}"))
                };
                (file, anchor)
            };
            found.push(Link {
                line: index + 1,
                target,
                file,
                anchor,
            });
        }
    }
    found
}

/// Each link in `doc` (named `name`) whose file or heading is missing.
fn link_errors(name: &str, doc: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for Link {
        line,
        target,
        file,
        anchor,
    } in links(name, doc)
    {
        let Ok(text) = fs::read_to_string(repo_file(&file)) else {
            if !repo_file(&file).exists() {
                errors.push(format!("{name}:{line}: {target}: no file {file}"));
            }
            continue;
        };
        if !anchor.is_empty() && file.ends_with(".md") && !anchors(&text).contains(anchor) {
            errors.push(format!("{name}:{line}: {target}: no heading #{anchor}"));
        }
    }
    errors
}

/// The index of the user docs, which links every page under docs/.
const INDEX: &str = "docs/README.md";

/// The pages among `pages` that `index` does not link, besides the index
/// itself.
fn unindexed(index: &str, pages: &[String]) -> Vec<String> {
    let linked: HashSet<String> = links(INDEX, index)
        .into_iter()
        .map(|link| link.file)
        .collect();
    pages
        .iter()
        .filter(|page| page.as_str() != INDEX && !linked.contains(page.as_str()))
        .cloned()
        .collect()
}

/// `path`, a repository path with `/` between its parts as links and the
/// messages here show it, with its `..` and `.` parts resolved.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            other => parts.push(other),
        }
    }
    parts.join("/")
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

#[test]
fn a_repeated_heading_gets_githubs_number() {
    let doc = "# Upgrade\n## Notes\n### Notes\n```\n## Notes\n```\n## Notes\n";
    let found = anchors(doc);
    for anchor in ["upgrade", "notes", "notes-1", "notes-2"] {
        assert!(found.contains(anchor), "#{anchor} missing from {found:?}");
    }
    // A heading inside a code block is no heading.
    assert!(!found.contains("notes-3"), "{found:?}");
}

// The index is how a reader finds a page, so a page it leaves out is one
// nobody finds.
#[test]
fn the_docs_index_links_every_page() {
    let pages: Vec<String> = docs()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("docs/"))
        .collect();
    let index = fs::read_to_string(repo_file(INDEX)).unwrap();
    let missing = unindexed(&index, &pages);
    assert!(
        missing.is_empty(),
        "{INDEX} does not link {}",
        missing.join(", ")
    );
}

#[test]
fn a_page_missing_from_the_index_is_caught() {
    let pages = ["docs/README.md", "docs/a.md", "docs/b.md", "docs/dev/c.md"].map(String::from);
    let index = "[A](a.md#part), [C](dev/c.md) and [up](../README.md)\n```\n[B](b.md)\n```\n";
    assert_eq!(unindexed(index, &pages), ["docs/b.md"]);
}

/// The folders whose text the example-id check reads: every text the
/// repository holds but the build output. The root's own files are read
/// too.
const ID_FOLDERS: [&str; 8] = [
    "docs",
    "skills",
    "release-notes",
    "src",
    "tests",
    "evals",
    "examples",
    ".github",
];

/// The kinds of file those folders hold text in.
const ID_EXTENSIONS: [&str; 11] = [
    "md", "rs", "sql", "json", "py", "toml", "yml", "yaml", "sh", "ps1", "addition",
];

/// Ids that tests need in a particular shape, each plainly made up, beside
/// the families `example_id` accepts. docs/dev/development.md lists them
/// with the same reasons.
const SHAPED_IDS: [&str; 10] = [
    // A version 1 UUID (RFC 9562's DNS namespace), which an import refuses.
    "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    // Version 4 with the Microsoft variant, which an import refuses too.
    "6ba7b810-9dad-41d1-c0b4-00c04fd430c8",
    // Not version 4.
    "00000000-0000-1000-8000-000000000003",
    // Version 4 with the wrong variant.
    "00000000-0000-4000-c000-000000000004",
    // Two ids sharing their short form.
    "abcdef12-0000-4000-8000-000000000001",
    "abcdef12-0000-4000-8000-000000000002",
    // Another memory, named only in a field that refers to one.
    "11111111-1111-4111-8111-111111111111",
    // An index entry left for a memory that is gone.
    "1a2b3c4d-0000-4000-8000-00000000dead",
    // Ids that sort after every other.
    "ffffffff-ffff-4fff-bfff-ffffffffffff",
    "ffffffff-0000-4000-8000-ffffffffffff",
];

/// Whether `digits` is `len` decimal digits.
fn decimal(digits: &str, len: usize) -> bool {
    digits.len() == len && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `id` is one an example may use: an `e` example id
/// (`e0000001-0000-4000-8000-000000000001`, the number in the first group
/// and again at the end), a schema seed's (`1a2b3c4d-1111-4aaa-8aaa-…01`),
/// a test's fixed id (`00000000-0000-4000-8000-0000000000NN`), or one of
/// `SHAPED_IDS`.
fn example_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let example = id
        .strip_prefix('e')
        .and_then(|rest| rest.split_once("-0000-4000-8000-00000"))
        .is_some_and(|(number, end)| decimal(number, 7) && number == end);
    let seed = id
        .strip_prefix("1a2b3c4d-")
        .and_then(|rest| rest.split_once("-4aaa-8aaa-0000000000"))
        .is_some_and(|(group, end)| decimal(group, 4) && decimal(end, 2));
    let fixed = id
        .strip_prefix("00000000-0000-4000-8000-0000000000")
        .is_some_and(|end| end.len() == 2 && end.bytes().all(|b| b.is_ascii_hexdigit()));
    example || seed || fixed || SHAPED_IDS.contains(&id.as_str())
}

/// Whether the byte at `at` in `text` starts or ends nothing longer: a
/// letter, digit or `-` joins it to its neighbour.
fn apart(text: &[u8], at: Option<usize>) -> bool {
    at.and_then(|at| text.get(at))
        .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'-')
}

/// The UUID-shaped strings in `text` that are not example ids, each with
/// its line.
fn stray_uuids(text: &str) -> Vec<(usize, String)> {
    /// Where a UUID has its hyphens.
    const HYPHENS: [usize; 4] = [8, 13, 18, 23];
    /// A UUID's length.
    const UUID: usize = 36;
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for start in 0..bytes.len().saturating_sub(UUID - 1) {
        let candidate = &bytes[start..start + UUID];
        let shaped = candidate.iter().enumerate().all(|(i, b)| {
            if HYPHENS.contains(&i) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        });
        if shaped
            && apart(bytes, start.checked_sub(1))
            && apart(bytes, Some(start + UUID))
            && let Ok(id) = std::str::from_utf8(candidate)
            && !example_id(id)
        {
            let line = text[..start].matches('\n').count() + 1;
            found.push((line, id.to_owned()));
        }
    }
    found
}

/// The short ids in a doc that are not example ids: 8 lowercase hex
/// characters with a letter among them, standing alone or cut from a full
/// id with `-…`. All digits is a date or a number, and a hash joined to a
/// name by a dot (`….e0e0e0e0`) is no id; a commit id is quoted at another
/// length.
fn stray_short_ids(text: &str) -> Vec<(usize, String)> {
    /// A short id's length.
    const SHORT: usize = 8;
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for start in 0..bytes.len().saturating_sub(SHORT - 1) {
        let candidate = &bytes[start..start + SHORT];
        let hex = candidate
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
        let joined = start
            .checked_sub(1)
            .is_some_and(|before| bytes[before] == b'.');
        // A full id cut short, as in `{"e0000001-…": …}`.
        let cut = bytes[start + SHORT..].starts_with("-…".as_bytes());
        if hex
            && !candidate.iter().all(u8::is_ascii_digit)
            && !joined
            && apart(bytes, start.checked_sub(1))
            && (cut || apart(bytes, Some(start + SHORT)))
            && let Ok(id) = std::str::from_utf8(candidate)
            && !(id.starts_with('e') && decimal(&id[1..], SHORT - 1))
        {
            let line = text[..start].matches('\n').count() + 1;
            found.push((line, id.to_owned()));
        }
    }
    found
}

/// Every text file under `folder`, by its path from the repository root.
fn texts(folder: &Path, root: &Path, into: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "__pycache__") {
                texts(&path, root, into);
            }
        } else if path
            .extension()
            .is_some_and(|ext| ID_EXTENSIONS.iter().any(|known| ext == *known))
        {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            into.push((name, fs::read_to_string(&path).unwrap()));
        }
    }
}

// Examples use one set of plainly made-up ids, the same few everywhere
// (docs/dev/development.md#example-ids-and-titles), so a reader or a tool
// never takes an example for a stored memory.
#[test]
fn every_uuid_is_an_example() {
    let root = repo_file("");
    let mut files = Vec::new();
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ID_EXTENSIONS.iter().any(|known| ext == *known))
        {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            files.push((name, fs::read_to_string(&path).unwrap()));
        }
    }
    for folder in ID_FOLDERS {
        texts(&root.join(folder), &root, &mut files);
    }
    assert!(files.iter().any(|(name, _)| name == "src/main.rs"));
    let mut strays: Vec<String> = files
        .iter()
        .flat_map(|(name, text)| {
            stray_uuids(text)
                .into_iter()
                .map(move |(line, id)| format!("{name}:{line}: {id}"))
        })
        .collect();
    for (name, doc) in docs() {
        strays.extend(
            stray_short_ids(&doc)
                .into_iter()
                .map(|(line, id)| format!("{name}:{line}: {id}")),
        );
    }
    assert!(
        strays.is_empty(),
        "ids outside the example set (docs/dev/development.md#example-ids-and-titles):\n{}",
        strays.join("\n")
    );
}

#[test]
fn an_id_outside_the_example_set_is_caught() {
    // Built here, so this file holds no stray id for the check above.
    let stray = ["c0ffee11", "2222", "4333", "8444", "555566667777"].join("-");
    let doc = "Run `user-memory read e0000001-0000-4000-8000-000000000001`.\n\
        {\"id\":\"STRAY\"}\n\
        ```output\n    e0000002 Use Python 3.13 in this repository [project]\n    c0ffee11 Answer briefly\n```\n\
        Undo `shared.json@2026-11-15T10:42:07Z.e0e0e0e0`; commit 3f9a1c2 or 3f9a1c2b4d; on 20261015.\n\
        {\"c0ffee22-…\": \"Answer briefly\"}, {\"e0000003-…\": \"Ship the parser by November\"}\n"
        .replace("STRAY", &stray);
    assert_eq!(stray_uuids(&doc), [(2, stray.clone())]);
    assert_eq!(
        stray_short_ids(&doc),
        [(5, "c0ffee11".to_owned()), (8, "c0ffee22".to_owned())]
    );
    let test = "const ID: &str = \"STRAY\";\n\
        let seed = \"1a2b3c4d-3333-4aaa-8aaa-000000000003\";\n\
        let fixed = \"00000000-0000-4000-8000-00000000000a\";\n\
        let longer = \"00000000-0000-4000-8000-000000000001-\";\n\
        let shaped = \"6ba7b810-9dad-11d1-80b4-00c04fd430c8\";\n"
        .replace("STRAY", &stray);
    assert_eq!(stray_uuids(&test), [(1, stray)]);
    // An `e` id whose numbers differ is no example id.
    assert!(!example_id(
        &["e0000001", "0000-4000-8000-000000000002"].join("-")
    ));
    assert!(example_id("E0000007-0000-4000-8000-000000000007"));
}
