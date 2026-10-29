#!/usr/bin/env python3
"""Checks of the eval harness that run no session: which commands Codex may
run outside its sandbox, and Claude's permission rules. Run with
`python3 evals/test_run.py`."""

import importlib.util
import re
import tempfile
import types
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run", HERE / "run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)


class Escalation(unittest.TestCase):
    def setUp(self):
        self.trial = Path(tempfile.mkdtemp()).resolve()
        self.repo = self.trial / "notebooks"
        self.repo.mkdir()

    def allowed(self, command, cwd=None):
        return run.escalation(command, str(cwd or self.repo), self.trial)

    def test_commits_with_reading_steps_are_approved(self):
        for command in [
            # The requests Codex sent in the eval batches.
            "/bin/zsh -lc \"git add README.md && git commit -m 'docs: add documentation build command' && git status --short\"",
            "/bin/zsh -lc \"git add README.md && git commit -m \\\"Document how to build the documentation\\\" && git status --short && git log -1 --format='%h %s'\"",
            "/bin/zsh -lc 'git add README.md && git commit -m \"docs: add documentation build instruction\" && git status --short'",
            "git add -A; git commit --message='docs: x'",
            "git commit -m 'docs: x' -m 'Why: the docs need a build command.'",
            "git add README.md && git commit -m x && git diff --stat --cached && git log -n 2 --oneline && git show --stat HEAD",
        ]:
            self.assertTrue(self.allowed(command), command)

    def test_anything_else_is_declined(self):
        for command in [
            # Options that write or read a file outside the trial.
            "git add . && git diff --output=/Users/x/.claude/CLAUDE.md",
            "git add . && git log -p --output=/Users/x/.zshrc",
            "git commit -F /Users/x/.codex/auth.json",
            "git commit --file=/Users/x/.codex/auth.json",
            "git commit --file /Users/x/.codex/auth.json",
            "git commit -m x --template=/Users/x/.ssh/id_rsa",
            "git add --pathspec-from-file=/Users/x/list",
            # Paths outside the trial.
            "git add /Users/x/.zshrc && git commit -m x",
            "git add ../../.. && git commit -m x",
            # Other repositories, pushes, shell features.
            "git -C /Users/x/repo commit -m x",
            "git --git-dir=/elsewhere/.git commit -m x",
            "git add . && git commit -m x && git push",
            "git push origin main",
            "git add . && git commit -m x | tee log",
            "git commit -m \"$(cat /Users/x/.codex/auth.json)\"",
            "git commit -m `whoami`",
            "git commit -m x > /tmp/out",
            "git commit -m x & sleep 1",
            "git status",
            "git log -1 && git diff",
            "rm -rf ~/.codex",
            "git commit -m 'unclosed",
            "",
            # Shell expansion after the path check (zsh -lc).
            "git add . && git diff {/Users/x/.codex/auth.json,README.md}",
            "git add . && git diff ~user/.codex/auth.json README.md",
            "git add . && git diff ~/x README.md",
            "git add . && git diff ../*/x",
            "git add =git",
        ]:
            self.assertFalse(self.allowed(command), command)

    def test_only_inside_the_trial(self):
        self.assertFalse(self.allowed("git commit -m x", cwd=Path.home()))
        self.assertFalse(run.escalation("git commit -m x", None, self.trial))

    def test_each_request_and_its_decision_is_logged(self):
        session = object.__new__(run.CodexSession)
        session.sandbox = types.SimpleNamespace(dir=self.trial)
        session.approvals, sent = [], []
        session.send = sent.append
        params = {"command": "git commit -m x", "cwd": str(self.repo)}
        session.answer({"id": 1, "method": "item/commandExecution/requestApproval", "params": params})
        session.answer({"id": 2, "method": "item/commandExecution/requestApproval",
                        "params": {**params, "command": "git push"}})
        session.answer({"id": 3, "method": "execCommandApproval",
                        "params": {**params, "command": ["git", "commit", "-m", "y"]}})
        session.answer({"id": 4, "method": "item/fileChange/requestApproval", "params": params})
        self.assertEqual([m["result"]["decision"] for m in sent],
                         ["accept", "decline", "approved", "decline"])
        self.assertEqual([a["decision"] for a in session.approvals],
                         ["accept", "decline", "approved", "decline"])
        # A declined push still counts as an attempted push.
        passed, _ = run.grade({"approvals_not": {"matches": "git\\b.*\\bpush"}}, None,
                              [{"approvals": session.approvals}], [])
        self.assertFalse(passed)


class Cleanup(unittest.TestCase):
    def test_a_codex_session_that_fails_to_start_leaves_no_login(self):
        home = Path(tempfile.mkdtemp())
        sandbox = types.SimpleNamespace(codex_home=home)

        def fail(self, cwd, options):
            raise RuntimeError("codex app-server ended during thread/start")
        original = run.CodexSession.start
        run.CodexSession.start = fail
        try:
            with self.assertRaises(RuntimeError):
                run.CodexSession(sandbox, home, {})
        finally:
            run.CodexSession.start = original
        self.assertFalse((home / "auth.json").is_symlink())

    def test_a_failed_turn_still_closes_every_open_session(self):
        closed = []

        class Session:
            def __init__(self, name):
                self.name = name

            def turn(self, prompt):
                if prompt == "fails":
                    raise RuntimeError("turn failed")
                return [], None

            def close(self):
                closed.append(self.name)
                return ""
        home = Path(tempfile.mkdtemp())
        sandbox = types.SimpleNamespace(codex_home=home, workdir=lambda spec, projects: home)
        names = iter(["a", "b"])
        scenario = {"name": "s", "checks": [], "steps": [
            {"session": "a", "prompt": "first"},
            {"session": "b", "prompt": "fails"},
            {"session": "a", "prompt": "never"}]}
        originals = run.prepare, run.open_session
        run.prepare = lambda *a: sandbox
        run.open_session = lambda *a: Session(next(names))
        try:
            with self.assertRaises(RuntimeError):
                run.trial(scenario, "claude", home, home, {},
                          types.SimpleNamespace(check=lambda *a: None))
        finally:
            run.prepare, run.open_session = originals
        self.assertEqual(sorted(closed), ["a", "b"])


def bash_rule_matches(rule, command):
    """Whether a `Bash(...)` rule matches one command, as Claude Code's
    permissions doc describes it: `*` stands for any text, a trailing ` *`
    also matches the bare command, and no `*` means an exact match."""
    pattern = rule[len("Bash("):-1]
    if pattern.endswith(" *") and pattern.count("*") == 1:
        return command == pattern[:-2] or command.startswith(pattern[:-1])
    return re.fullmatch(".*".join(map(re.escape, pattern.split("*"))), command) is not None


class ClaudeRules(unittest.TestCase):
    trial = Path("/Users/u/Library/Caches/user-memory-eval/trials/b/trial-claude-x")

    def rules(self):
        return run.claude_shell_rules(self.trial, self.trial / "notebooks", ["README.md"])

    def allowed(self, command):
        allow, deny = self.rules()
        bash = lambda rules: [r for r in rules if r.startswith("Bash(")]  # noqa: E731
        return (any(bash_rule_matches(r, command) for r in bash(allow))
                and not any(bash_rule_matches(r, command) for r in bash(deny)))

    def test_edits_stay_in_the_project(self):
        allow, deny = self.rules()
        self.assertEqual([r for r in allow if r.startswith("Edit")],
                         [f"Edit(/{self.trial}/notebooks/**)"])
        for folder in ("bin", "home", "tools", "state"):
            self.assertIn(f"Edit(/{self.trial}/{folder}/**)", deny)

    def test_only_exact_git_shapes_are_allowed(self):
        for command in ["git status", "git status --short", "git add .", "git add -A",
                        "git add README.md", "git commit -m 'docs: add the docs build line'",
                        # git ignores a template next to -m.
                        "git commit -m x --template=/Users/x/.ssh/id_rsa"]:
            self.assertEqual(self.allowed(command),
                             "--template" not in command, command)
        project = self.trial / "notebooks"
        for command in ["git commit -qaF/Users/x/.codex/auth.json",
                        "git commit -a --fil=/Users/x/.codex/auth.json",
                        "git commit --templ=/Users/x/.ssh/id_rsa -m x",
                        "git commit -F /Users/x/secret",
                        "git commit -q -m x --amend",
                        "git commit --amend -m x",
                        f"git -C {project} commit -q -m x --amend",
                        f"git -C {project} commit -q -m x -F /Users/x/secret",
                        "git -C /Users/x/other commit -q -m x",
                        f"git -C {self.trial} commit -q -m x",
                        "git add --pathspec-from-f=/Users/x/list",
                        "git add /Users/x/.zshrc",
                        "git diff --output=/Users/x/.zshrc",
                        "git log --output=/Users/x/.zshrc",
                        "git push", "git push origin main",
                        "git status --porcelain --output=/x"]:
            self.assertFalse(self.allowed(command), command)


    def test_the_commits_claude_ran_in_the_eval_are_allowed(self):
        project = self.trial / "notebooks"
        # Each part of the compound command; Claude Code allows the
        # read-only git log and git status parts itself.
        for command in ["git add README.md",
                        'git commit -q -m "Add docs build instructions to README"',
                        "git status --short",
                        f'git -C {project} commit -q -m "Add docs build instructions to README" -- README.md',
                        f'git -C {project} commit -q -am "Add docs build instructions to README"',
                        'git commit -am "docs: add the build line"',
                        f"git -C {project} add README.md"]:
            self.assertTrue(self.allowed(command), command)


class CodexClose(unittest.TestCase):
    def test_codex_stops_before_its_login_is_removed(self):
        home = Path(tempfile.mkdtemp())
        link = home / "auth.json"
        seen = []

        class Child:
            def kill(self):
                seen.append(("kill", link.is_symlink()))

            def wait(self):
                seen.append(("wait", link.is_symlink()))
        session = object.__new__(run.CodexSession)
        session.sandbox = types.SimpleNamespace(codex_home=home)
        session.child = Child()
        session.errors = run.queue.Queue()
        link.symlink_to(home / "missing")
        session.close()
        self.assertEqual(seen, [("kill", True), ("wait", True)])
        self.assertFalse(link.is_symlink())


class ResumeAndCompact(unittest.TestCase):
    def test_a_resume_reopens_the_same_conversation_in_a_new_process(self):
        opened, closed, compacted = [], [], []

        class Session:
            def __init__(self, resume):
                self.resume = resume

            def turn(self, prompt):
                event = {"type": "result", "session_id": "conv-1", "result": prompt}
                return [event], None

            def compact(self):
                compacted.append(self.resume)
                return [], None

            def close(self):
                closed.append(self.resume)
                return ""

        home = Path(tempfile.mkdtemp())
        (home / "space").mkdir()
        sandbox = types.SimpleNamespace(codex_home=home, dir=home, memories=lambda: [], setup_output="",
                                        workdir=lambda spec, projects: home)
        scenario = {"name": "s", "checks": [], "watch": ["space/.user-memory"], "steps": [
            {"session": "a", "prompt": "link it"},
            {"session": "a", "compact": True},
            {"session": "a", "resume": True, "prompt": "use it"}]}

        def open_session(client, sandbox, cwd, options, resume=None):
            opened.append((resume, options["resumes"], options["compacts"]))
            return Session(resume)
        originals = run.prepare, run.open_session
        run.prepare = lambda *a: sandbox
        run.open_session = open_session
        try:
            result = run.trial(scenario, "claude", home, home, {},
                               types.SimpleNamespace(check=lambda *a: None))
        finally:
            run.prepare, run.open_session = originals
        self.assertEqual(opened, [(None, True, True), ("conv-1", True, True)])
        self.assertEqual(compacted, [None])
        self.assertEqual(closed, [None, "conv-1"])
        self.assertEqual([s["prompt"] for s in result["steps"]], ["link it", "/compact", "use it"])
        self.assertEqual([s["resumed"] for s in result["steps"]], [False, False, True])
        self.assertEqual(result["steps"][0]["files"], {"space/.user-memory": None})

    def test_a_resume_counts_as_one_more_session(self):
        scenario = {"steps": [{"session": "a", "prompt": "x"}, {"session": "a", "resume": True, "prompt": "y"},
                              {"session": "b", "prompt": "z"}]}
        self.assertEqual(run.session_opens(scenario), 3)


class FileCheck(unittest.TestCase):
    def test_a_step_sees_the_file_as_it_was_then_and_the_end_as_it_is(self):
        trial = Path(tempfile.mkdtemp())
        (trial / "space").mkdir()
        (trial / "space/.user-memory").write_text("@notebooks\n")
        sandbox = types.SimpleNamespace(dir=trial)
        records = [{"files": {"space/.user-memory": None}}]

        def grade(spec):
            return run.grade({"file": spec}, sandbox, records, [])[0]
        self.assertTrue(grade({"step": 0, "path": "space/.user-memory", "absent": True}))
        self.assertFalse(grade({"path": "space/.user-memory", "absent": True}))
        self.assertTrue(grade({"path": "space/.user-memory", "matches": "^@notebooks/?$"}))
        self.assertFalse(grade({"path": "space/.user-memory", "matches": "^@compiler/?$"}))
        self.assertFalse(grade({"step": 0, "path": "space/.user-memory", "matches": "."}))


class CodexStart(unittest.TestCase):
    def test_a_codex_session_starts_with_the_trials_environment(self):
        seen = {}

        class Stop(Exception):
            pass

        def popen(command, cwd, env, **kwargs):
            seen["env"], seen["cwd"] = env, cwd
            raise Stop

        home = Path(tempfile.mkdtemp())
        sandbox = types.SimpleNamespace(env={"HOME": str(home)}, codex_home=home)
        session = run.CodexSession.__new__(run.CodexSession)
        session.sandbox, session.resume, session.child = sandbox, None, None
        original = run.subprocess.Popen, run.codex_binary
        run.subprocess.Popen, run.codex_binary = popen, lambda: "codex"
        try:
            with self.assertRaises(Stop):
                session.start(home, {})
        finally:
            run.subprocess.Popen, run.codex_binary = original
        self.assertEqual((seen["env"], seen["cwd"]), (sandbox.env, home))


class TrialPruning(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp()).resolve()

    def batch(self, name):
        folder = self.root / name
        (folder / "trial-claude-x/home").mkdir(parents=True)
        (folder / "trial-claude-x/home/store").write_text("memories")
        return folder

    def names(self):
        return sorted(p.name for p in self.root.iterdir())

    def test_a_new_batch_keeps_only_the_newest_ones(self):
        for name in ["20261017-141725", "20261017-152039", "20261019-060406"]:
            self.batch(name)
        original = run.TRIALS
        run.TRIALS = self.root
        try:
            trials = run.start_trials("20261029-120000", keep=2)
        finally:
            run.TRIALS = original
        self.assertEqual(trials, self.root / "20261029-120000")
        self.assertEqual(self.names(), ["20261019-060406", "20261029-120000"])
        self.assertEqual(trials.stat().st_mode & 0o777, 0o700)

    def test_fewer_batches_than_kept_are_left_alone(self):
        for name in ["20261017-141725", "20261019-060406"]:
            self.batch(name)
        self.assertEqual(run.prune_trials(self.root, keep=2), [])
        self.assertEqual(self.names(), ["20261017-141725", "20261019-060406"])

    def test_only_the_harnesss_own_batch_folders_are_removed(self):
        for name in ["20261017-141725", "20261019-060406"]:
            self.batch(name)
        outside = Path(tempfile.mkdtemp()).resolve()
        (outside / "keep.txt").write_text("not the harness's")
        (self.root / "20261001-000000").symlink_to(outside)
        (self.root / "20261002-000000.log").write_text("a file")
        (self.root / "20261003-000000").write_text("a file named like a batch")
        (self.root / "notes").mkdir()
        removed = run.prune_trials(self.root, keep=1)
        self.assertEqual(removed, ["20261017-141725"])
        self.assertEqual(self.names(), ["20261001-000000", "20261002-000000.log", "20261003-000000",
                                        "20261019-060406", "notes"])
        self.assertTrue((outside / "keep.txt").exists())

    def test_read_only_files_and_folders_in_a_batch_are_removed(self):
        old = self.batch("20261017-141725")
        self.batch("20261019-060406")
        objects = old / "trial-claude-x/project/.git/objects/ab"
        objects.mkdir(parents=True)
        (objects / "cdef").write_text("object")
        (objects / "cdef").chmod(0o444)
        objects.chmod(0o555)
        locked = old / "trial-claude-x/home/locked"
        (locked / "inside").mkdir(parents=True)
        locked.chmod(0o000)
        (old / "README").write_text("a file at the top")
        old.chmod(0o500)
        self.assertEqual(run.prune_trials(self.root, keep=1), ["20261017-141725"])
        self.assertEqual(self.names(), ["20261019-060406"])

    def test_a_link_out_of_a_batch_leaves_its_target_as_it_was(self):
        old = self.batch("20261017-141725")
        self.batch("20261019-060406")
        outside = Path(tempfile.mkdtemp()).resolve()
        outside.chmod(0o755)
        (old / "trial-claude-x/home/link").symlink_to(outside)
        run.prune_trials(self.root, keep=1)
        self.assertEqual(self.names(), ["20261019-060406"])
        self.assertEqual(outside.stat().st_mode & 0o777, 0o755)

    def test_the_kept_count_comes_from_the_environment(self):
        self.assertEqual(run.kept_batches({}), run.KEPT_BATCHES)
        self.assertEqual(run.kept_batches({"USER_MEMORY_EVAL_KEEP_TRIALS": "7"}), 7)
        for bad in ["0", "-1", "all", ""]:
            with self.assertRaises(SystemExit, msg=bad):
                run.kept_batches({"USER_MEMORY_EVAL_KEEP_TRIALS": bad})


if __name__ == "__main__":
    unittest.main()
