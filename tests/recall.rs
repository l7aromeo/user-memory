//! Recall through the task (#117), through the real binary: after a tool
//! call, the post-tool hook pushes the memories the call matches, each
//! labelled by whose word it is, once per agent context and stretch.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::HOOK_ROOM;

use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::{TempDir, tempdir};

/// Opens every push after a call that did not fail.
const HEADER: &str =
    "From user-memory, matching this step (if one applies, read it for the rest first):";

/// A sandboxed home with its own store, its sessions working in it.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> PathBuf {
        self.path().join("memory.sqlite3")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(self.path())
            .sandboxed(self.path())
            .env("USER_MEMORY_DB", self.db())
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        command
    }

    /// Records a global memory with `args` after its title and content;
    /// returns its id.
    fn record(&self, title: &str, content: &str, args: &[&str]) -> String {
        let out = self.record_reply(title, content, args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    /// [`Home::record`]'s reply, stored or not.
    fn record_reply(&self, title: &str, content: &str, args: &[&str]) -> Value {
        let typed = args.contains(&"--type");
        let mut all = vec!["record"];
        if !typed {
            all.extend(["--type", "procedure"]);
        }
        all.extend([
            "--category",
            "build",
            "--title",
            title,
            "--content",
            content,
        ]);
        if !args.contains(&"--scope") {
            all.extend(["--scope", "global"]);
        }
        all.extend(args);
        let out = self.command(&all).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Runs `subcommand` for Claude Code with `payload` on stdin; its
    /// stdout.
    fn hook(&self, subcommand: &str, payload: &Value) -> String {
        let mut child = self
            .command(&[subcommand, "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, subcommand);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// Starts `session` as `source` says.
    fn start(&self, session: &str, source: &str) {
        self.hook(
            "session-start",
            &json!({"session_id": session, "cwd": self.path(), "hook_event_name": "SessionStart", "source": source}),
        );
    }

    /// The push after `tool` was called with `input` in `session`, by
    /// the agent context `agent` when given; empty for none.
    fn call(
        &self,
        session: &str,
        agent: Option<&str>,
        tool: &str,
        input: Value,
        response: Value,
    ) -> String {
        let mut payload = json!({"session_id": session, "cwd": self.path(), "hook_event_name": "PostToolUse",
                                 "tool_name": tool, "tool_input": input, "tool_response": response});
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
            payload["agent_type"] = json!("general-purpose");
        }
        let out = self.hook("post-tool-use", &payload);
        if out.is_empty() {
            return out;
        }
        let out: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["hookEventName"], "PostToolUse",
            "{out}"
        );
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The push `client`'s post-tool hook prints for `payload`, and the
    /// event it names; empty for none.
    fn push(&self, client: &str, payload: &Value) -> (String, String) {
        let mut child = self
            .command(&["post-tool-use", "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, "post-tool-use");
        assert!(out.status.success(), "{out:?}");
        if out.stdout.is_empty() {
            return (String::new(), String::new());
        }
        let out: Value = serde_json::from_slice(&out.stdout).unwrap();
        let context = &out["hookSpecificOutput"];
        (
            context["hookEventName"].as_str().unwrap().to_owned(),
            context["additionalContext"].as_str().unwrap().to_owned(),
        )
    }

    /// Starts Codex's `session` and returns what its start printed.
    fn start_codex(&self, session: &str) -> String {
        let mut child = self
            .command(&["session-start", "--client", "codex"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = json!({"session_id": session, "cwd": self.path(), "hook_event_name": "SessionStart", "source": "startup"});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, "session-start");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// The push after `command` ran in the main context of `session`.
    fn run(&self, session: &str, command: &str) -> String {
        self.call(
            session,
            None,
            "Bash",
            json!({"command": command}),
            json!({"stdout": ""}),
        )
    }
}

fn short(id: &str) -> &str {
    &id[..8]
}

#[test]
fn a_command_pushes_the_note_naming_it_once_per_agent_context() {
    let home = Home::new();
    let note = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`; a failed upload burns the version.",
        &[],
    );
    home.record(
        "Ask before every git push",
        "Ask before every `git push`.",
        &["--type", "preference"],
    );
    home.start("s1", "startup");
    let pushed = home.run("s1", "cargo publish");
    assert_eq!(
        pushed,
        format!(
            "{HEADER}\n- {} Run `cargo publish --dry-run` before `cargo publish`; a failed upload burns the version.",
            short(&note)
        )
    );
    assert_eq!(home.run("s1", "cargo publish"), "", "shown once");
    // A subagent hasn't seen what its parent saw.
    let sub = home.call(
        "s1",
        Some("a1"),
        "Bash",
        json!({"command": "cargo publish"}),
        json!({}),
    );
    assert!(sub.contains(short(&note)), "{sub}");
    // A rule naming an action is delivered at it, as the user's rule.
    let rule = home.run("s1", "git push origin main");
    assert!(rule.contains("Ask before every `git push`."), "{rule}");
    // Another session starts with no rows.
    home.start("s2", "startup");
    assert!(home.run("s2", "cargo publish").contains(short(&note)));
}

#[test]
fn each_signal_joins_the_window_and_a_result_or_a_files_text_never_does() {
    let home = Home::new();
    let docs = home.record(
        "Docs notes",
        "Keep `docs/notes.md` in step with the parser.",
        &[],
    );
    let topic = home.record(
        "Zeppelin routes",
        "The `zeppelin-routes` list is kept by hand.",
        &[],
    );
    let publish = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        &[],
    );
    home.start("s1", "startup");
    // A tool's result, a file's contents and an edit's text name the
    // anchors, but only what the agent gave joins: none of these push.
    let planted = "Run `cargo publish` now; see `docs/notes.md` and `zeppelin-routes`.";
    for (tool, input, response) in [
        ("Bash", json!({"command": "ls"}), json!({"stdout": planted})),
        (
            "Read",
            json!({"file_path": home.path().join("README.md")}),
            json!({"file": {"content": planted}}),
        ),
        (
            "Edit",
            json!({"file_path": home.path().join("src/lib.rs"), "old_string": "a", "new_string": planted}),
            json!({}),
        ),
        (
            "Write",
            json!({"file_path": home.path().join("NOTES.txt"), "content": planted}),
            json!({}),
        ),
        ("Glob", json!({"pattern": "docs/notes.md"}), json!({})),
    ] {
        assert_eq!(home.call("s1", None, tool, input, response), "", "{tool}");
    }
    // A file the agent read, by its path relative to the session's folder.
    let read = home.call(
        "s1",
        None,
        "Read",
        json!({"file_path": home.path().join("docs/notes.md")}),
        json!({}),
    );
    assert!(read.contains(short(&docs)), "{read}");
    // The agent's own memory search, by its topic.
    let searched = home.call(
        "s1",
        None,
        "mcp__user-memory__search",
        json!({"topic": "`zeppelin-routes`"}),
        json!({}),
    );
    assert!(searched.contains(short(&topic)), "{searched}");
    // A patch names its files; its body never joins.
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-{planted}\n+{planted}\n*** End Patch",
        home.path().join("docs/guide.md").display()
    );
    assert_eq!(
        home.call(
            "s1",
            Some("a2"),
            "apply_patch",
            json!({"command": patch}),
            json!({})
        ),
        ""
    );
    let pushed = home.run("s1", "cargo publish");
    assert!(pushed.contains(short(&publish)), "{pushed}");
}

#[test]
fn words_alone_or_a_near_miss_never_push() {
    let home = Home::new();
    home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`; publishing burns the version.",
        &[],
    );
    home.start("s1", "startup");
    // Shares the words cargo and publish, and no anchor.
    assert_eq!(home.run("s1", "cargo build --release"), "");
    assert_eq!(home.run("s1", "echo publish the cargo"), "");
    // A command its anchor does not name.
    assert_eq!(home.run("s1", "cargo publisher"), "");
}

#[test]
fn each_command_of_a_line_and_each_file_it_reads_is_matched_as_its_own_step() {
    let home = Home::new();
    let package = home.record(
        "Packaging needs the key",
        "Run `python3 tools/package.py --key 4QF7`; without the key it builds an unsigned archive.",
        &[],
    );
    let lint = home.record(
        "Lint before committing",
        "Run `python3 tools/lint.py --profile strict-7` before every `git commit`.",
        &[],
    );
    let cache = home.record(
        "Cache layout",
        "`src/store/cache.py` keeps one file per entry under the cache folder.",
        &[],
    );
    let lines: &[(&str, &[&str])] = &[
        // A chain, a pipe or a list of reads, each naming the note's file
        // or command among others: pushed as after that part alone.
        (
            "cat RELEASING.md; echo ---; cat tools/package.py",
            &[&package],
        ),
        ("cat README.md src/a.py tools/package.py", &[&package]),
        ("cat a.md b.md c.md tools/package.py", &[&package]),
        (
            "cat tools/package.py src/a.py src/b.py src/c.py",
            &[&package],
        ),
        (
            "head -n 40 RELEASING.md tools/package.py | less",
            &[&package],
        ),
        (
            "(cd tools && ls) && python3 tools/package.py 2>&1 | tail -20",
            &[&package],
        ),
        (
            "echo 'one' >> notes.txt && python3 tools/lint.py --profile strict-7",
            &[&lint],
        ),
        (
            "git add notes.txt && git commit -m \"Add one to notes\"",
            &[&lint],
        ),
        // A commit message's words and paths are no part of the step.
        (
            "git commit -m \"Add one to notes.txt\n\nCo-Authored-By: someone\"",
            &[&lint],
        ),
        (
            "cat MILESTONES.md README.md src/store/cache.py tests/test_store.py tools/package.py",
            &[&cache, &package],
        ),
        // A line naming none of them pushes nothing, nor does a read of
        // other files or a message naming one.
        ("echo ---; ls -la | wc -l", &[]),
        ("cat README.md src/a.py src/b.py src/c.py", &[]),
        (
            "git commit -m \"Run tools/package.py and src/store/cache.py\"",
            &[&lint],
        ),
    ];
    for (n, (line, expected)) in lines.iter().enumerate() {
        let session = format!("s{n}");
        home.start(&session, "startup");
        let pushed = home.run(&session, line);
        for id in [&package, &lint, &cache] {
            assert_eq!(
                pushed.contains(short(id)),
                expected.contains(&id.as_str()),
                "{line}: {id} in {pushed:?}"
            );
        }
    }
}

#[test]
fn a_here_document_is_text_the_command_reads_unless_a_shell_runs_it() {
    let home = Home::new();
    let suite = home.record(
        "Only the supervisor runs the e2e suite",
        "Workers never run `bun test:e2e`; it loads the suite's token.",
        &[],
    );
    let cache = home.record(
        "Store contract tests",
        "CI collects only `test_<name>_contract` functions for code under `src/store/`.",
        &[],
    );
    let read = format!("cat {}", home.path().join("src/store/cache.py").display());
    let lines: Vec<(String, Vec<&str>)> = vec![
        // A body is what the command reads, as quoted text is: its lines
        // name no command and no file of the step, and the line goes on
        // after its delimiter, however that is written.
        (
            "cat > notes/msg.txt <<'EOF'\ndocs: say the suite passes\n\nbun test:e2e passes.\nEOF"
                .into(),
            vec![],
        ),
        (
            format!("cat <<EOF > notes/a.txt\nbun test:e2e\nEOF\n{read}"),
            vec![cache.as_str()],
        ),
        (
            format!("cat <<'EOF'\nbun test:e2e\nEOF\n{read}"),
            vec![cache.as_str()],
        ),
        (
            format!("cat << \"EOF\"\nbun test:e2e\nEOF\n{read}"),
            vec![cache.as_str()],
        ),
        (
            format!("cat <<\\EOF\nbun test:e2e\nEOF\n{read}"),
            vec![cache.as_str()],
        ),
        (
            format!("cat <<-EOF\n\tbun test:e2e\n\tEOF\n{read}"),
            vec![cache.as_str()],
        ),
        (
            format!("cat <<EOF\r\nbun test:e2e\r\nEOF\r\n{read}"),
            vec![cache.as_str()],
        ),
        (format!("python3 - <<'PY'\n{read}\nPY"), vec![]),
        // Two on one line: each body in turn.
        (
            format!("cat <<A <<'B'; echo done\none\nA\nbun test:e2e\nB\n{read}"),
            vec![cache.as_str()],
        ),
        // A body no line ends runs to the end of the command.
        ("cat <<EOF\nbun test:e2e".into(), vec![]),
        // `<<` that opens none: quoted, a here-string, or a shift.
        ("echo \"<<EOF\"\nbun test:e2e".into(), vec![suite.as_str()]),
        ("cat <<< EOF\nbun test:e2e".into(), vec![suite.as_str()]),
        (
            "echo $((1 << 2))\nbun test:e2e\n2".into(),
            vec![suite.as_str()],
        ),
        // A shell reads it as its script: its commands are the step's,
        // their paths taken relative to the session's folder too.
        (
            "bash <<'EOF'\nbun test:e2e\nEOF".into(),
            vec![suite.as_str()],
        ),
        (
            "sh -s notes.txt <<EOF\nset -e\nbun test:e2e\nEOF".into(),
            vec![suite.as_str()],
        ),
        (
            format!("bash -e <<'EOF'\n{read}\nEOF"),
            vec![cache.as_str()],
        ),
        // Not when the shell runs a script or its `-c`: the body is data.
        (
            "bash tools/seed.sh <<'EOF'\nbun test:e2e\nEOF".into(),
            vec![],
        ),
        ("bash -c 'cat' <<'EOF'\nbun test:e2e\nEOF".into(), vec![]),
    ];
    for (n, (line, expected)) in lines.iter().enumerate() {
        let session = format!("s{n}");
        home.start(&session, "startup");
        let pushed = home.run(&session, line);
        for id in [&suite, &cache] {
            assert_eq!(
                pushed.contains(short(id)),
                expected.contains(&id.as_str()),
                "{line:?}: {id} in {pushed:?}"
            );
        }
    }
}

#[test]
fn a_file_read_through_the_shell_reaches_what_a_read_of_it_reaches() {
    let home = Home::new();
    let store = home.record(
        "Store contract tests",
        "CI collects only `test_<name>_contract` functions for code under `src/store/`.",
        &[],
    );
    let read = |session: &str| {
        home.start(session, "startup");
        home.call(
            session,
            None,
            "Read",
            json!({"file_path": home.path().join("src/store/cache.py")}),
            json!({}),
        )
    };
    assert!(read("r1").contains(short(&store)), "a read");
    // By its path relative to the session's folder, or from the root, as a
    // read takes it.
    let absolute = format!("cat {}", home.path().join("src/store/cache.py").display());
    for (n, line) in [
        "cat src/store/cache.py",
        "sed -n 1,40p src/store/cache.py",
        "cat MILESTONES.md src/store/cache.py",
        absolute.as_str(),
    ]
    .iter()
    .enumerate()
    {
        let session = format!("s{n}");
        home.start(&session, "startup");
        let pushed = home.run(&session, line);
        assert!(pushed.contains(short(&store)), "{line}: {pushed:?}");
    }
    home.start("other", "startup");
    assert_eq!(home.run("other", "cat src/parse/lexer.py"), "");
}

#[test]
fn a_windows_path_read_through_the_shell_reaches_what_a_read_of_it_reaches() {
    let home = Home::new();
    let store = home.record(
        "Store contract tests",
        "CI collects only `test_<name>_contract` functions for code under `src/store/`.",
        &[],
    );
    // On Windows the session's folder and the paths in it name their
    // folders with `\`, in any case, and a command may write either `\`
    // or `/`.
    let call = |session: &str, tool: &str, input: Value| {
        home.start(session, "startup");
        let payload = json!({"session_id": session, "cwd": r"C:\Users\dev\proj",
                             "hook_event_name": "PostToolUse", "tool_name": tool,
                             "tool_input": input, "tool_response": {}});
        home.push("claude", &payload).1
    };
    let read = call(
        "r1",
        "Read",
        json!({"file_path": r"C:\Users\dev\proj\src\store\cache.py"}),
    );
    assert!(read.contains(short(&store)), "a read: {read:?}");
    // Claude Code's Bash tool on Windows runs Git Bash, which writes the
    // drive as a folder.
    for (n, (tool, line)) in [
        (
            "PowerShell",
            r"Get-Content C:\Users\dev\proj\src\store\cache.py",
        ),
        ("PowerShell", r"type c:\users\dev\proj\src\store\cache.py"),
        ("PowerShell", r"Get-Content src\store\cache.py"),
        (
            "PowerShell",
            r"& C:\tools\rg.exe contract src\store\cache.py",
        ),
        ("Bash", "cat C:/Users/dev/proj/src/store/cache.py"),
        ("Bash", "cat /c/Users/dev/proj/src/store/cache.py"),
    ]
    .iter()
    .enumerate()
    {
        let pushed = call(&format!("p{n}"), tool, json!({"command": line}));
        assert!(pushed.contains(short(&store)), "{line}: {pushed:?}");
    }
    let other = call(
        "other",
        "PowerShell",
        json!({"command": r"Get-Content C:\Users\dev\proj\src\parse\lexer.py"}),
    );
    assert_eq!(other, "");
}

#[test]
fn an_agents_note_is_labelled_as_one() {
    let home = Home::new();
    let note = home.record(
        "Release script needs a tag",
        "`./scripts/release.sh` fails without a tag on the commit.",
        &[
            "--type",
            "lesson",
            "--source",
            "observed",
            "--evidence",
            "the release script exited 1 on an untagged commit",
            "--origin",
            "claude:session/e0000005",
        ],
    );
    // An agent's note of a rule's type could be read as the user's wish.
    let habit = home.record(
        "Deploys run verbosely",
        "Run `./scripts/deploy.sh` with `--verbose`, so a failure shows its step.",
        &[
            "--type",
            "preference",
            "--source",
            "observed",
            "--evidence",
            "the user reran the deploy script with --verbose after it failed",
            "--origin",
            "claude:session/e0000006",
        ],
    );
    home.start("s1", "startup");
    // A note about the work is marked as a note; one of a rule's type says
    // it is not the user's rule.
    let pushed = home.run("s1", "./scripts/release.sh");
    assert!(
        pushed.contains(&format!(
            "- {} An agent's note: `./scripts/release.sh` fails",
            short(&note)
        )),
        "{pushed}"
    );
    let pushed = home.run("s1", "./scripts/deploy.sh");
    assert!(
        pushed.contains(&format!(
            "- {} An agent's note, not the user's rule: Run `./scripts/deploy.sh`",
            short(&habit)
        )),
        "{pushed}"
    );
}

#[test]
fn a_long_memory_pushes_its_matching_passage_only() {
    let home = Home::new();
    let filler = |topic: &str| {
        format!("{topic}: ").to_owned()
            + &"The parser keeps its tables sorted and its errors short. ".repeat(7)
    };
    let content = format!(
        "{}\n\n{}\n\nBefore a release run `cargo publish --dry-run` and read its warnings.\n\n{}",
        filler("Tables"),
        filler("Errors"),
        filler("Warnings")
    );
    let id = home.record("Parser upkeep", &content, &[]);
    home.start("s1", "startup");
    let pushed = home.run("s1", "cargo publish --dry-run");
    let line = pushed.strip_prefix(&format!("{HEADER}\n- {} ", short(&id)));
    let passage = line.unwrap_or_else(|| panic!("{pushed}"));
    assert!(
        passage.contains("Before a release run `cargo publish --dry-run` and read its warnings."),
        "{passage}"
    );
    // A passage, not the memory: the first paragraph stays out, and the
    // line says there is more.
    assert!(!passage.contains("Tables:"), "{passage}");
    assert!(passage.ends_with(" …"), "{passage}");
    assert!(passage.chars().count() < 500, "{passage}");
}

/// A memory of three passages, each naming a command no other names:
/// each paragraph is too long to share a passage with the next.
const THREE_STEPS: &str = "Pack the release with `numbat-pack --all` first. It gathers every crate the workspace builds, checks that each one has a version the registry has not seen, and writes one archive beside the manifest so the next steps read a single file. A crate it cannot gather stops the step with its name, and the archive is written again from the start once that crate builds.\n\n\
Then sign that archive with `bilby-sign --key release`. The mirror refuses an archive with no signature, and an upload it refuses has to start again from the beginning, which on a slow line takes most of an afternoon to finish. The key lives on the release machine only, so the step runs there and nowhere else, and a copy of the key on a laptop is revoked.\n\n\
Last, ship the signed archive with `dingo-ship --channel stable`, and only once the signature checks out on a second machine, since a version shipped to the stable channel can never be taken back or replaced by another. A version that has to be withdrawn is marked as such on the channel, and the fix ships under the next version number after it.";

// A text push shows the passage that matches, so each passage of one
// memory is shown once, at the step that names it.
#[test]
fn each_passage_of_a_memory_is_pushed_at_the_step_naming_it() {
    let home = Home::new();
    let id = home.record("Release in three steps", THREE_STEPS, &[]);
    home.start("s1", "startup");
    for (command, passage) in [
        ("numbat-pack --all", "`numbat-pack --all`"),
        ("bilby-sign --key release", "`bilby-sign --key release`"),
        (
            "dingo-ship --channel stable",
            "`dingo-ship --channel stable`",
        ),
    ] {
        let pushed = home.run("s1", command);
        let line = pushed
            .strip_prefix(&format!("{HEADER}\n- {} ", short(&id)))
            .unwrap_or_else(|| panic!("{command}: {pushed}"));
        assert!(line.contains(passage), "{command}: {line}");
        assert!(!line.contains('\n'), "one line: {pushed}");
    }
    assert_eq!(
        home.run("s1", "numbat-pack --all"),
        "",
        "each passage once in a stretch"
    );
}

impl Home {
    /// Makes the home a checkout of `github.com/you/app`.
    fn checkout(&self) {
        for args in [
            &["init", "-q"][..],
            &["remote", "add", "origin", "https://github.com/you/app.git"],
        ] {
            let out = Command::new("git")
                .args(args)
                .current_dir(self.path())
                .sandboxed(self.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
    }
}

#[test]
fn session_start_opens_epochs_and_its_rules_in_full_count_as_shown() {
    let home = Home::new();
    home.checkout();
    // A project rule naming a command, which session start prints in full.
    home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        &[
            "--type",
            "preference",
            "--scope",
            "project",
            "--project",
            "github.com/you/app",
        ],
    );
    // An agent's note of a wish, which the most-used part never prints
    // (#123): only pushes bring it.
    let note = home.record(
        "Check before committing",
        "Run `just check` before every commit.",
        &[
            "--type",
            "preference",
            "--source",
            "observed",
            "--evidence",
            "The user ran it before each commit",
        ],
    );
    home.start("s1", "startup");
    assert_eq!(
        home.run("s1", "cargo publish"),
        "",
        "printed in full at the start"
    );
    assert!(home.run("s1", "just check").contains(short(&note)));
    // resume: nothing shown before is pushed again.
    home.start("s1", "resume");
    assert_eq!(home.run("s1", "just check"), "");
    // compact: what was shown is gone from the context, but the rule is
    // printed again.
    home.start("s1", "compact");
    assert!(home.run("s1", "just check").contains(short(&note)));
    assert_eq!(home.run("s1", "cargo publish"), "");
    // clear: every passage can be pushed again.
    home.start("s1", "clear");
    assert!(home.run("s1", "just check").contains(short(&note)));
}

/// Opens every push after a call that failed.
const FAILED_HEADER: &str =
    "From user-memory, matching this failed step (if one applies, read it for the rest first):";

#[test]
fn after_a_claude_code_failure_the_anchored_note_is_pushed_as_a_failure() {
    let home = Home::new();
    let trap = home.record(
        "Release script needs a tag",
        "`./scripts/release.sh` fails without a tag on the commit.",
        &[],
    );
    home.record(
        "Releases need care",
        "Scripts for a release fail when a commit lacks its tag.",
        &[],
    );
    home.start("s1", "startup");
    let failure = json!({"session_id": "s1", "cwd": home.path(), "hook_event_name": "PostToolUseFailure",
                         "tool_name": "Bash", "tool_input": {"command": "./scripts/release.sh"},
                         "error": "Exit code 1", "is_interrupt": false});
    let (event, pushed) = home.push("claude", &failure);
    assert_eq!(event, "PostToolUseFailure");
    // The note naming the command, and not the one sharing its words only.
    assert_eq!(
        pushed,
        format!(
            "{FAILED_HEADER}\n- {} `./scripts/release.sh` fails without a tag on the commit.",
            short(&trap)
        )
    );
}

#[test]
fn a_codex_failure_is_read_from_its_transcript_and_never_its_output() {
    let home = Home::new();
    let trap = home.record(
        "Release script needs a tag",
        "`./scripts/release.sh` fails without a tag on the commit.",
        &[],
    );
    let publish = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        &[],
    );
    home.start_codex("c1");
    let transcript = home.path().join("rollout.jsonl");
    let call = |id: &str, status: &str, exit_code: i64| {
        // The command's output names the other note's command.
        let line = json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {
            "type": "CommandExecution", "id": id, "status": status, "exit_code": exit_code,
            "aggregated_output": "error: run `cargo publish` from a tagged commit"}}});
        std::fs::write(&transcript, format!("{line}\n")).unwrap();
        json!({"session_id": "c1", "cwd": home.path(), "hook_event_name": "PostToolUse",
               "tool_name": "Bash", "tool_use_id": id, "transcript_path": transcript,
               "tool_input": {"command": "./scripts/release.sh"},
               "tool_response": "error: run `cargo publish` from a tagged commit"})
    };
    let (event, pushed) = home.push("codex", &call("exec-e0000001", "failed", 1));
    assert_eq!(event, "PostToolUse");
    assert!(pushed.starts_with(FAILED_HEADER), "{pushed}");
    assert!(pushed.contains(short(&trap)), "{pushed}");
    assert!(
        !pushed.contains(short(&publish)),
        "the output never joins: {pushed}"
    );
    // A later call that succeeded pushes under the plain header, and its
    // output still never joins.
    home.record(
        "Release notes go with the tag",
        "After `./scripts/release.sh` succeeds, write the release notes.",
        &[],
    );
    home.start_codex("c2");
    let (_, pushed) = home.push("codex", &{
        let mut payload = call("exec-e0000002", "completed", 0);
        payload["session_id"] = json!("c2");
        payload
    });
    assert!(pushed.starts_with(HEADER), "{pushed}");
    assert!(!pushed.contains(short(&publish)), "{pushed}");
}

impl Home {
    /// Runs `subcommand` for `client` with `payload`; the event and the
    /// text it prints, empty for none.
    fn run_hook(&self, subcommand: &str, client: &str, payload: &Value) -> (String, String) {
        let mut child = self
            .command(&[subcommand, "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, subcommand);
        assert!(out.status.success(), "{out:?}");
        if out.stdout.is_empty() {
            return (String::new(), String::new());
        }
        let out: Value = serde_json::from_slice(&out.stdout).unwrap();
        let context = &out["hookSpecificOutput"];
        (
            context["hookEventName"].as_str().unwrap().to_owned(),
            context["additionalContext"].as_str().unwrap().to_owned(),
        )
    }
}

#[test]
fn a_subagent_starts_with_its_task_and_its_own_context() {
    let home = Home::new();
    let docs = home.record(
        "Docs notes",
        "Keep `docs/notes.md` in step with the parser.",
        &[],
    );
    home.start("s1", "startup");
    let spawn = |prompt_id: &str, kind: &str, prompt: &str| {
        json!({"session_id": "s1", "cwd": home.path(), "hook_event_name": "PreToolUse", "prompt_id": prompt_id,
               "tool_name": "Agent", "tool_use_id": format!("t-{kind}"),
               "tool_input": {"prompt": prompt, "description": "Docs", "subagent_type": kind}})
    };
    let started = |prompt_id: &str, agent: &str, kind: &str| {
        json!({"session_id": "s1", "cwd": home.path(), "hook_event_name": "SubagentStart",
               "prompt_id": prompt_id, "agent_id": agent, "agent_type": kind})
    };
    home.run_hook(
        "pre-tool-use",
        "claude",
        &spawn("p1", "Explore", "Update `docs/notes.md` for the parser."),
    );
    // A subagent of another turn or type does not take it.
    assert_eq!(
        home.run_hook("subagent-start", "claude", &started("p2", "a0", "Explore"))
            .1,
        ""
    );
    assert_eq!(
        home.run_hook("subagent-start", "claude", &started("p1", "a0", "Plan"))
            .1,
        ""
    );
    let (event, pushed) =
        home.run_hook("subagent-start", "claude", &started("p1", "a1", "Explore"));
    assert_eq!(event, "SubagentStart");
    assert!(pushed.starts_with(HEADER), "{pushed}");
    assert!(pushed.contains(short(&docs)), "{pushed}");
    // Shown to the subagent once; its parent has not seen it.
    let read = json!({"file_path": home.path().join("docs/notes.md")});
    assert_eq!(
        home.call("s1", Some("a1"), "Read", read.clone(), json!({})),
        ""
    );
    assert!(
        home.call("s1", None, "Read", read, json!({}))
            .contains(short(&docs))
    );
    // A call naming no type starts Claude Code's default one.
    let mut untyped = spawn("p3", "Explore", "Update `docs/notes.md` for the parser.");
    untyped["tool_input"]
        .as_object_mut()
        .unwrap()
        .remove("subagent_type");
    home.run_hook("pre-tool-use", "claude", &untyped);
    let pushed = home
        .run_hook(
            "subagent-start",
            "claude",
            &started("p3", "a3", "general-purpose"),
        )
        .1;
    assert!(pushed.contains(short(&docs)), "{pushed}");
}

#[test]
fn a_codex_sub_agent_claims_the_oldest_task_of_its_session() {
    let home = Home::new();
    let docs = home.record(
        "Docs notes",
        "Keep `docs/notes.md` in step with the parser.",
        &[],
    );
    let release = home.record(
        "Release script needs a tag",
        "`./scripts/release.sh` fails without a tag on the commit.",
        &[],
    );
    home.start_codex("c1");
    let spawn = |message: &str| {
        json!({"session_id": "c1", "cwd": home.path(), "hook_event_name": "PreToolUse", "turn_id": "u1",
               "tool_name": "collaborationspawn_agent", "tool_use_id": "call_1",
               "tool_input": {"message": message, "task_name": "task"}})
    };
    let started = |agent: &str| {
        json!({"session_id": "c1", "cwd": home.path(), "hook_event_name": "SubagentStart",
               "turn_id": format!("turn-{agent}"), "agent_id": agent, "agent_type": "default"})
    };
    // Two spawned in one turn: nothing ties a start to its call, so each
    // start takes the oldest task left, whichever sub-agent it is.
    home.run_hook(
        "pre-tool-use",
        "codex",
        &spawn("Update `docs/notes.md` for the parser."),
    );
    home.run_hook(
        "pre-tool-use",
        "codex",
        &spawn("Run `./scripts/release.sh` for the tag."),
    );
    let first = home.run_hook("subagent-start", "codex", &started("x2")).1;
    let second = home.run_hook("subagent-start", "codex", &started("x1")).1;
    assert!(
        first.contains(short(&docs)) && !first.contains(short(&release)),
        "{first}"
    );
    assert!(
        second.contains(short(&release)) && !second.contains(short(&docs)),
        "{second}"
    );
    assert_eq!(
        home.run_hook("subagent-start", "codex", &started("x3")).1,
        ""
    );
}

/// A home in a checkout of github.com/you/app holding one project rule,
/// to run `cargo publish --dry-run` first, and that rule's id.
fn publish_rule() -> (Home, String) {
    let home = Home::new();
    home.checkout();
    let rule = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        &[
            "--type",
            "preference",
            "--scope",
            "project",
            "--project",
            "github.com/you/app",
        ],
    );
    (home, rule)
}

#[test]
fn a_rule_a_subagents_start_prints_in_full_is_not_pushed_to_it_again() {
    let (home, rule) = publish_rule();
    let publish = json!({"command": "cargo publish"});
    // Claude Code: the subagent's start prints the rule in full, and its
    // task names the rule's command.
    home.start("s1", "startup");
    home.run_hook(
        "pre-tool-use",
        "claude",
        &json!({"session_id": "s1", "cwd": home.path(), "hook_event_name": "PreToolUse", "prompt_id": "p1",
                "tool_name": "Agent", "tool_use_id": "t-1",
                "tool_input": {"prompt": "Run `cargo publish` for the release.", "description": "Release",
                               "subagent_type": "general-purpose"}}),
    );
    let (_, started) = home.run_hook(
        "subagent-start",
        "claude",
        &json!({"session_id": "s1", "cwd": home.path(), "hook_event_name": "SubagentStart",
                "prompt_id": "p1", "agent_id": "a1", "agent_type": "general-purpose"}),
    );
    assert_eq!(
        started.matches(short(&rule)).count(),
        1,
        "printed in full, not pushed below it: {started}"
    );
    assert_eq!(
        home.call("s1", Some("a1"), "Bash", publish, json!({})),
        "",
        "nor at the subagent's step"
    );
}

#[test]
fn a_rule_a_forked_codex_sub_agent_carries_is_not_pushed_to_it_again() {
    let (home, rule) = publish_rule();
    let publish = json!({"command": "cargo publish"});
    // A forked sub-agent carries its parent's session start, rules
    // included, so its start prints nothing and they count as shown.
    let rules = home.start_codex("c1");
    assert!(rules.contains(short(&rule)), "{rules}");
    let forked = home.path().join("forked.jsonl");
    let said = json!({"type": "response_item", "payload": {"type": "message", "role": "developer",
                      "content": [{"type": "input_text", "text": rules}]}});
    std::fs::write(&forked, format!("{said}\n")).unwrap();
    assert_eq!(
        home.run_hook(
            "subagent-start",
            "codex",
            &json!({"session_id": "c1", "cwd": home.path(), "hook_event_name": "SubagentStart",
                    "turn_id": "turn-x1", "agent_id": "x1", "agent_type": "default",
                    "transcript_path": forked}),
        )
        .1,
        ""
    );
    let (_, pushed) = home.push(
        "codex",
        &json!({"session_id": "c1", "cwd": home.path(), "hook_event_name": "PostToolUse",
                "tool_name": "Bash", "tool_use_id": "call_1", "agent_id": "x1",
                "tool_input": publish, "tool_response": ""}),
    );
    assert_eq!(pushed, "", "carried from the parent");
}

#[test]
fn a_parents_words_to_a_subagent_join_its_window_and_the_reply_never_does() {
    let home = Home::new();
    let docs = home.record(
        "Docs notes",
        "Keep `docs/notes.md` in step with the parser.",
        &[],
    );
    let publish = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        &[],
    );
    home.start_codex("c1");
    let message = json!({"session_id": "c1", "cwd": home.path(), "hook_event_name": "PostToolUse",
                         "tool_name": "collaborationsend_message", "tool_use_id": "call_2",
                         "tool_input": {"target": "/root/docs", "message": "Also check `docs/notes.md`."},
                         "tool_response": "Done; ran `cargo publish`."});
    let (_, pushed) = home.run_hook("post-tool-use", "codex", &message);
    assert!(pushed.contains(short(&docs)), "{pushed}");
    assert!(!pushed.contains(short(&publish)), "{pushed}");
}

impl Home {
    /// The text the prompt hook adds before `prompt` in `session`; empty
    /// for none.
    fn prompt(&self, session: &str, prompt: &str) -> String {
        let payload = json!({"session_id": session, "cwd": self.path(),
                             "hook_event_name": "UserPromptSubmit", "prompt": prompt});
        let out = self.hook("prompt-submit", &payload);
        if out.is_empty() {
            return out;
        }
        let out: Value = serde_json::from_str(&out).unwrap();
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[test]
fn a_prompt_pushes_what_its_words_name_after_the_notice_and_a_task_notification_does_not() {
    let home = Home::new();
    let docs = home.record(
        "Docs notes",
        "Keep `docs/notes.md` in step with the parser.",
        &[],
    );
    home.start("s1", "startup");
    // A subagent's result delivered as a prompt is not the user's words.
    let notification =
        "<task-notification>\n<result>Updated `docs/notes.md`.</result>\n</task-notification>";
    assert_eq!(home.prompt("s1", notification), "");
    // A memory recorded since the start: the notice first, then recall.
    let later = home.record("Parser tables", "Keep `src/tables.rs` sorted.", &[]);
    let said = home.prompt("s1", "Please bring `docs/notes.md` up to date.");
    let (notice, recalled) = said
        .split_once(&format!("\n\n{HEADER}\n"))
        .unwrap_or_else(|| panic!("{said}"));
    assert!(notice.contains(short(&later)), "{said}");
    assert_eq!(
        recalled,
        format!(
            "- {} Keep `docs/notes.md` in step with the parser.",
            short(&docs)
        )
    );
    // Shown in this stretch: the next step on that file pushes nothing.
    let read = json!({"file_path": home.path().join("docs/notes.md")});
    assert_eq!(home.call("s1", None, "Read", read, json!({})), "");
    // Words alone never push.
    assert_eq!(
        home.prompt("s1", "Keep the notes in step with the parser."),
        ""
    );
}

impl Home {
    /// Runs the CLI with `args`, which must succeed.
    fn cli(&self, args: &[&str]) {
        let out = self.command(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

#[test]
fn a_memory_recorded_changed_or_forgotten_mid_turn_reaches_the_next_call() {
    let home = Home::new();
    home.record("Parser tables", "Keep `src/tables.rs` sorted.", &[]);
    home.start("s1", "startup");
    assert_eq!(home.run("s1", "cargo publish"), "");
    // Recorded by another process after the session started: matched at
    // the next call.
    let note = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        &[],
    );
    assert!(home.run("s1", "cargo publish").contains(short(&note)));
    // Changed to name another command: matched on its new words only.
    home.cli(&[
        "update",
        &note,
        "--content",
        "Run `just release` before tagging.",
    ]);
    assert_eq!(home.run("s2", "cargo publish"), "");
    home.start("s3", "startup");
    home.cli(&[
        "update",
        &note,
        "--content",
        "Run `just release-check` before tagging.",
    ]);
    assert_eq!(home.run("s3", "cargo publish"), "", "its old words");
    let pushed = home.run("s3", "just release-check");
    assert!(
        pushed.contains("Run `just release-check` before tagging."),
        "{pushed}"
    );
    // Forgotten: never pushed again, in any context.
    home.cli(&["forget", &note]);
    let sub = home.call(
        "s3",
        Some("a1"),
        "Bash",
        json!({"command": "just release-check"}),
        json!({}),
    );
    assert_eq!(sub, "");
}

#[test]
fn a_note_on_a_script_comes_however_the_script_is_run() {
    let home = Home::new();
    let note = home.record(
        "Step two reads its region",
        "`tools/step2.sh` reads region2.conf and stops when it is stale.",
        &[
            "--type",
            "fact",
            "--source",
            "observed",
            "--evidence",
            "Seen in two sessions",
        ],
    );
    let script = home.record(
        "The build script needs its cache",
        "`tools/build.py` reads build/cache.json and stops when it is missing.",
        &[
            "--type",
            "fact",
            "--source",
            "observed",
            "--evidence",
            "Seen in two sessions",
        ],
    );
    // A session each, so a push in one says nothing of the others.
    let mut missed = Vec::new();
    for (n, (command, id)) in [
        ("tools/step2.sh", &note),
        ("./tools/step2.sh", &note),
        ("bash tools/step2.sh", &note),
        ("sh -e tools/step2.sh --all", &note),
        ("zsh ./tools/step2.sh", &note),
        ("python3 tools/build.py", &script),
        ("python3 -u ./tools/build.py --fast", &script),
        ("bash -c 'tools/step2.sh'", &note),
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("s{n}");
        home.start(&session, "startup");
        if !home.run(&session, command).contains(short(id)) {
            missed.push(command);
        }
    }
    assert!(missed.is_empty(), "no push after {missed:?}");
    for (n, command) in [
        "bash tools/other.sh",
        "python3 tools/other.py",
        "./tools/other.sh",
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("n{n}");
        home.start(&session, "startup");
        assert_eq!(home.run(&session, command), "", "{command}");
    }
}

#[test]
fn a_note_writing_the_script_with_its_dot_slash_comes_however_it_is_run() {
    let home = Home::new();
    let note = home.record(
        "Deploy needs its target",
        "`./tools/deploy.sh` reads deploy/target and stops when it is unset.",
        &[
            "--type",
            "fact",
            "--source",
            "observed",
            "--evidence",
            "Seen in two sessions",
        ],
    );
    let mut missed = Vec::new();
    for (n, command) in [
        "./tools/deploy.sh",
        "tools/deploy.sh",
        "bash tools/deploy.sh",
        "bash ./tools/deploy.sh",
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("s{n}");
        home.start(&session, "startup");
        if !home.run(&session, command).contains(short(&note)) {
            missed.push(command);
        }
    }
    assert!(missed.is_empty(), "no push after {missed:?}");
}

#[test]
fn a_note_on_a_script_at_the_folder_root_comes_however_it_is_run() {
    let home = Home::new();
    let note = home.record(
        "Build needs its toolchain",
        "`./build.sh` reads toolchain.txt and stops when it is missing.",
        &[
            "--type",
            "fact",
            "--source",
            "observed",
            "--evidence",
            "Seen in two sessions",
        ],
    );
    let mut missed = Vec::new();
    for (n, command) in ["./build.sh", "bash ./build.sh", "sh build.sh", "build.sh"]
        .into_iter()
        .enumerate()
    {
        let session = format!("s{n}");
        home.start(&session, "startup");
        if !home.run(&session, command).contains(short(&note)) {
            missed.push(command);
        }
    }
    assert!(missed.is_empty(), "no push after {missed:?}");
}

#[test]
fn a_note_on_a_program_comes_when_the_program_is_written_with_its_exe() {
    let home = Home::new();
    let note = home.record(
        "Search here",
        "`rg` here is ripgrep 14, which skips ignored files.",
        &[
            "--type",
            "fact",
            "--source",
            "observed",
            "--evidence",
            "Seen in two sessions",
        ],
    );
    let mut missed = Vec::new();
    for (n, command) in ["rg -n foo", "rg.exe -n foo", "RG.EXE -n foo"]
        .into_iter()
        .enumerate()
    {
        let session = format!("s{n}");
        home.start(&session, "startup");
        if !home.run(&session, command).contains(short(&note)) {
            missed.push(command);
        }
    }
    assert!(missed.is_empty(), "no push after {missed:?}");
}

#[test]
fn a_note_on_the_interpreter_still_comes_when_it_runs_a_script() {
    let home = Home::new();
    let note = home.record(
        "Python here is 3.13",
        "In this checkout `python3` is 3.13; `python` is an older one that the scripts do not support.",
        &["--type", "fact", "--source", "observed", "--evidence", "Seen in two sessions"],
    );
    home.start("s1", "startup");
    let pushed = home.run("s1", "python3 tools/build.py");
    assert!(pushed.contains(short(&note)), "{pushed:?}");
}

#[test]
fn a_memory_whose_date_passed_mid_session_is_never_pushed() {
    time::date_window(|date| {
        let home = Home::new();
        let Some(note) = date.stored(&home.record_reply(
            "Dry run before publishing",
            "Run `cargo publish --dry-run` before `cargo publish`.",
            &["--until-date", &date.until()],
        )) else {
            return false;
        };
        home.start("s1", "startup");
        if !date.ahead(home.run("s1", "cargo publish").contains(short(&note))) {
            return false;
        }
        // A passed date writes nothing, so the delivery index still holds
        // the memory; the push reads it in force or not at all.
        date.wait_past();
        let sub = home.call(
            "s1",
            Some("a1"),
            "Bash",
            json!({"command": "cargo publish"}),
            json!({}),
        );
        assert_eq!(sub, "");
        true
    });
}
