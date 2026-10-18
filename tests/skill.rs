//! The user-memory skill names only tools, parameters, values, result fields
//! and CLI flags that this binary really has.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::{TempDir, tempdir};
use uuid::Uuid;

fn skill_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/user-memory")
}

/// (file name, contents) of every Markdown file in the skill.
fn skill_files() -> Vec<(String, String)> {
    let mut files: Vec<_> = fs::read_dir(skill_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read_to_string(&path).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn the_skill_has_valid_frontmatter_and_links_only_to_its_own_files() {
    let files = skill_files();
    let names: BTreeSet<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "SKILL.md",
            "backup.md",
            "forget.md",
            "projects.md",
            "review.md",
            "upgrade.md"
        ])
    );
    let skill = &files.iter().find(|(name, _)| name == "SKILL.md").unwrap().1;
    let frontmatter = skill
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .expect("SKILL.md starts with a --- frontmatter block")
        .0;
    let field = |key: &str| {
        frontmatter
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}: ")))
            .unwrap_or_else(|| panic!("frontmatter has no {key}"))
    };
    let name = field("name");
    assert_eq!(name, "user-memory");
    assert!(name.len() <= 64);
    let description = field("description");
    assert!(!description.is_empty() && description.chars().count() <= 1024);
    assert!(description.contains("Not for ordinary recall or recording"));
    for (file, text) in &files {
        for (_, rest) in text.match_indices("](").map(|(i, _)| ("", &text[i + 2..])) {
            let target = &rest[..rest.find(')').unwrap()];
            assert!(names.contains(target), "{file} links to {target}");
        }
    }
}

/// What the binary offers: tools with their parameters, values that
/// parameters take, fields of real tool results and review kinds, CLI
/// subcommands with their help text.
struct Surface {
    tools: BTreeMap<String, BTreeSet<String>>,
    values: BTreeSet<String>,
    results: BTreeSet<String>,
    commands: BTreeMap<String, String>,
}

impl Surface {
    fn parameter(&self, word: &str) -> bool {
        self.tools.values().any(|params| params.contains(word))
    }
    fn known(&self, word: &str) -> bool {
        self.tools.contains_key(word)
            || self.parameter(word)
            || self.values.contains(word)
            || self.results.contains(word)
            || self.commands.contains_key(word)
    }
}

fn binary(home: &Path, db: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    command
        .sandboxed(home)
        .env("USER_MEMORY_DB", db)
        .env("USER_MEMORY_APP_DIRS", "")
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE")
        .env_remove("USER_MEMORY_EMBED_URL");
    command
}

fn help(home: &Path, db: &Path, args: &[&str]) -> String {
    let out = binary(home, db).args(args).arg("--help").output().unwrap();
    assert!(out.status.success(), "{args:?} --help");
    String::from_utf8(out.stdout).unwrap()
}

/// Every string in `enum` or `const` anywhere in a JSON schema.
fn schema_values(schema: &Value, found: &mut BTreeSet<String>) {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("enum", Value::Array(items)) => {
                        found.extend(items.iter().filter_map(Value::as_str).map(str::to_owned))
                    }
                    ("const", Value::String(s)) => {
                        found.insert(s.clone());
                    }
                    _ => schema_values(value, found),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| schema_values(item, found)),
        _ => {}
    }
}

/// Every object key anywhere in `value`.
fn keys(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                found.insert(key.clone());
                keys(value, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| keys(item, found)),
        _ => {}
    }
}

fn entry(content: &str, category: &str, extra: Value) -> Value {
    let mut memory = json!({
        "id": Uuid::new_v4(), "type": "preference", "category": category, "title": content, "content": content,
        "context": "", "reason": "", "scope": "global", "project": "", "source": "explicit",
        "status": "confirmed", "evidence": "", "evidence_ids": [],
        "origins": [{"origin": "claude:session/a", "first_seen": "2026-10-15 05:20:11"}],
        "superseded_by": null, "created_at": "2026-10-15 05:20:11",
        "updated_at": "2026-10-15 05:20:11", "last_confirmed_at": null,
    });
    for (key, value) in extra.as_object().unwrap() {
        memory[key] = value.clone();
    }
    memory
}

/// A store holding one group of every review kind, imported from an export.
fn seed(home: &Path, db: &Path) {
    let global = entry("Use Helix as the text editor", "tools", json!({}));
    let memories = json!([
        global,
        entry(
            "Use Helix as the text editor",
            "tools",
            json!({"scope": "project", "project": "lab"})
        ),
        entry("Uses Fish as the interactive shell", "shell", json!({})),
        entry(
            "Uses Fish as the interactive shell please",
            "shell",
            json!({})
        ),
        // Eight more copies: a group too large to list, which says how many
        // it left out.
        entry("Uses Fish as the interactive shell 1", "shell", json!({})),
        entry("Uses Fish as the interactive shell 2", "shell", json!({})),
        entry("Uses Fish as the interactive shell 3", "shell", json!({})),
        entry("Uses Fish as the interactive shell 4", "shell", json!({})),
        entry("Uses Fish as the interactive shell 5", "shell", json!({})),
        entry("Uses Fish as the interactive shell 6", "shell", json!({})),
        entry("Uses Fish as the interactive shell 7", "shell", json!({})),
        entry("Uses Fish as the interactive shell 8", "shell", json!({})),
        entry("Use Helix as the text editor", "editor", json!({})),
        entry("Helix editor keybindings", "editor", json!({})),
        entry(
            "Run the linter before every commit",
            "workflow",
            json!({"scope": "project", "project": "lab"})
        ),
        entry(
            "Run the linter before every commit",
            "workflow",
            json!({"scope": "project", "project": "shop"})
        ),
        entry(
            "Learns tools by reading their manuals",
            "learning",
            json!({"type": "pattern", "source": "inferred", "status": "stale",
                   "evidence_ids": [global["id"], Uuid::new_v4()]})
        ),
    ]);
    let document = json!({"format": "user-memory-export", "format_version": 2,
                          "schema_version": 3, "memories": memories, "distinct_pairs": []});
    let mut child = binary(home, db)
        .args(["import", "-", "--apply"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(document.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Drives `serve` through `tools/list` and `calls`; returns the tool list and
/// each call's parsed result.
fn serve(home: &Path, db: &Path, calls: &[(&str, Value)]) -> (Vec<Value>, Vec<Value>) {
    let mut child = binary(home, db)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut send = |id: usize, method: &str, params: Value| {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        if method == "initialize" {
            writeln!(
                input,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
        }
        input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "no reply to {method}"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    };
    send(
        0,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"skill-test","version":"1"}}),
    );
    let tools = send(1, "tools/list", json!({}))["result"]["tools"]
        .as_array()
        .unwrap()
        .clone();
    let results = calls
        .iter()
        .enumerate()
        .map(|(i, (name, arguments))| {
            let reply = send(
                i + 2,
                "tools/call",
                json!({"name":name,"arguments":arguments}),
            );
            let text = reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{name}: {reply}"));
            serde_json::from_str(text).unwrap_or_else(|_| panic!("{name}: {text}"))
        })
        .collect();
    drop(input);
    let _ = child.wait();
    (tools, results)
}

fn surface(root: &TempDir) -> Surface {
    let (home, db) = (root.path(), root.path().join("store/memories.sqlite3"));
    seed(home, &db);
    let (tools, results) = serve(
        home,
        &db,
        &[
            (
                "review",
                json!({"projects": ["lab", "shop"], "max_groups": 20}),
            ),
            ("export", json!({"name": "backup.json"})),
            ("import", json!({"name": "backup.json"})),
            (
                "import",
                json!({"name": "backup.json", "apply": true, "user_confirmed": true}),
            ),
            // Filtered: the fields a filter adds to export and import.
            ("export", json!({"name": "shell.json", "category": "shell"})),
            ("import", json!({"name": "shell.json"})),
            (
                "import",
                json!({"name": "backup.json", "category": "shell"}),
            ),
            (
                "forget",
                json!({"origin": "claude:session/a", "user_confirmed": true}),
            ),
        ],
    );
    let mut values = BTreeSet::new();
    let mut tool_params = BTreeMap::new();
    for tool in &tools {
        let schema = &tool["inputSchema"];
        let params = schema["properties"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        tool_params.insert(tool["name"].as_str().unwrap().to_owned(), params);
        schema_values(schema, &mut values);
    }
    let mut fields = BTreeSet::new();
    for result in &results {
        keys(result, &mut fields);
    }
    let kinds: BTreeSet<String> = results[0]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["duplicate", "overlap", "redundant", "shared", "stale"].map(str::to_owned)),
        "the seeded store has one group of every kind: {}",
        results[0]
    );
    fields.extend(kinds);
    let top = help(home, &db, &[]);
    let commands = top
        .split_once("Commands:\n")
        .unwrap()
        .1
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|command| *command != "help")
        .map(|command| (command.to_owned(), help(home, &db, &[command])))
        .collect();
    Surface {
        tools: tool_params,
        values,
        results: fields,
        commands,
    }
}

/// Text between backticks on `line`, in order.
fn spans(line: &str) -> Vec<&str> {
    line.split('`').skip(1).step_by(2).collect()
}

fn identifier(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Problems with one `user-memory …` command line.
fn check_command(surface: &Surface, command: &str, problems: &mut Vec<String>) {
    let Some((_, rest)) = command.split_once("user-memory ") else {
        return;
    };
    let mut words = rest.split_whitespace();
    // `…` stands for any command, as in "run it through docker compose".
    let Some(sub) = words.next().filter(|sub| *sub != "…") else {
        return;
    };
    let Some(help) = surface.commands.get(sub) else {
        problems.push(format!("unknown subcommand `{sub}` in `{command}`"));
        return;
    };
    for word in words.take_while(|w| *w != "#") {
        // Optional parts are written in brackets: `[--limit N]`.
        let word = word.trim_matches(|c| matches!(c, '[' | ']'));
        if let Some(flag) = word.strip_prefix("--") {
            let flag = flag.split('=').next().unwrap();
            let listed = help
                .split(|c: char| c.is_whitespace() || c == ',')
                .any(|w| w == format!("--{flag}"));
            if !listed {
                problems.push(format!("`user-memory {sub}` has no --{flag}"));
            }
        } else if word.contains('|') {
            for value in word.split('|') {
                if !help.contains(value) {
                    problems.push(format!("`user-memory {sub}` takes no value {value}"));
                }
            }
        }
    }
}

#[test]
fn every_tool_parameter_value_and_command_the_skill_names_exists() {
    let root = tempdir().unwrap();
    let surface = surface(&root);
    let mut problems = Vec::new();
    for (file, text) in skill_files() {
        let mut fence = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                fence = !fence;
                continue;
            }
            if fence {
                check_command(&surface, line, &mut problems);
                continue;
            }
            // Parameters belong to the tool named before them on the line.
            let mut tool: Option<&str> = None;
            for span in spans(line) {
                if span.contains("user-memory ") {
                    check_command(&surface, span, &mut problems);
                    continue;
                }
                let (word, value) = match span.split_once(": ") {
                    Some((word, value)) => (word, Some(value)),
                    None => (span, None),
                };
                if !identifier(word) {
                    continue;
                }
                if surface.tools.contains_key(word) {
                    tool = Some(word);
                    continue;
                }
                if !surface.known(word) {
                    problems.push(format!(
                        "{file}: `{word}` is no tool, parameter, value or result field"
                    ));
                    continue;
                }
                if let Some(tool) = tool
                    && surface.parameter(word)
                    && !surface.tools[tool].contains(word)
                    && !surface.results.contains(word)
                {
                    problems.push(format!("{file}: `{tool}` has no parameter `{word}`"));
                }
                if let Some(value) = value {
                    let ok = matches!(value, "true" | "false") || surface.values.contains(value);
                    if !surface.parameter(word) || !ok {
                        problems.push(format!("{file}: `{span}` is not a parameter and value"));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
