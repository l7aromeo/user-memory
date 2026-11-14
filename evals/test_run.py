#!/usr/bin/env python3
"""Checks of the eval harness that run no session: which commands Codex may
run outside its sandbox, and Claude's permission rules. Run with
`python3 evals/test_run.py`."""

import importlib.util
import json
import re
import tempfile
import threading
import time
import types
import urllib.request
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run", HERE / "run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)
fake_model = run.fake_model


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



class Embedding(unittest.TestCase):
    def test_trials_get_semantic_search_only_when_an_endpoint_is_given(self):
        self.assertEqual(run.embed_settings({}), {})
        settings = run.embed_settings({
            "USER_MEMORY_EVAL_EMBED_URL": "http://127.0.0.1:11434/v1/embeddings",
            "USER_MEMORY_EVAL_EMBED_MODEL": "qwen3-embedding:4b",
            "USER_MEMORY_EVAL_EMBED_DIMENSIONS": "1024",
            "USER_MEMORY_EVAL_EMBED_QUERY_INSTRUCTION": "Instruct: find\\nQuery:",
        })
        self.assertEqual(settings, {
            "USER_MEMORY_EMBED_URL": "http://127.0.0.1:11434/v1/embeddings",
            "USER_MEMORY_EMBED_MODEL": "qwen3-embedding:4b",
            "USER_MEMORY_EMBED_DIMENSIONS": "1024",
            "USER_MEMORY_EMBED_QUERY_INSTRUCTION": "Instruct: find\\nQuery:",
            # Plain http reaches only this machine's endpoint.
            "USER_MEMORY_EMBED_ALLOW_HTTP": "1",
        })
        https = run.embed_settings({"USER_MEMORY_EVAL_EMBED_URL": "https://api.example.com/v1",
                                    "USER_MEMORY_EVAL_EMBED_MODEL": "small"})
        self.assertNotIn("USER_MEMORY_EMBED_ALLOW_HTTP", https)
        # A model with no endpoint is a mistake, not keyword-only search.
        with self.assertRaises(SystemExit):
            run.embed_settings({"USER_MEMORY_EVAL_EMBED_MODEL": "small"})


class Models(unittest.TestCase):
    def test_each_batch_runs_the_cheaper_models_unless_told(self):
        self.assertEqual(run.models(False, {}), ("sonnet", "gpt-6-luna"))
        self.assertEqual(run.models(True, {}), ("opus", run.CODEX_TOP_MODEL))
        self.assertNotEqual(run.CODEX_TOP_MODEL, run.CODEX_DEFAULT_MODEL)
        # The environment names a model for one run, whatever the tier.
        environ = {"USER_MEMORY_EVAL_CLAUDE_MODEL": "opus", "USER_MEMORY_EVAL_CODEX_MODEL": "gpt-6-astra"}
        self.assertEqual(run.models(False, environ), ("opus", "gpt-6-astra"))
        self.assertEqual(run.models(True, {"USER_MEMORY_EVAL_CLAUDE_MODEL": ""}),
                         ("opus", run.CODEX_TOP_MODEL))

    def test_a_superseded_or_missing_codex_model_stops_the_batch(self):
        listed = [{"id": "gpt-6-luna", "model": "gpt-6-luna", "upgrade": None},
                  {"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "upgrade": None},
                  {"id": "gpt-5.6-luna", "model": "gpt-5.6-luna", "upgrade": "gpt-6-luna"}]
        self.assertEqual(run.stale_models(listed, ["gpt-6-luna", "gpt-6.1-sol", "gpt-6-luna"]), [])
        self.assertEqual(run.stale_models(listed, ["gpt-5.6-luna", "gpt-6.1-sol"]), [
            "Codex reports gpt-5.6-luna superseded by gpt-6-luna: name the newer model in evals/run.py"])
        self.assertEqual(run.stale_models(listed, ["gpt-6-luna", "gpt-4"]), ["Codex lists no model gpt-4"])


class Usage(unittest.TestCase):
    # As Claude Code 2.1 reports them: `modelUsage` in a result event counts
    # every turn of the conversation so far.
    @staticmethod
    def claude_result(input_tokens, cost):
        return {"type": "result", "usage": {"input_tokens": 1000}, "total_cost_usd": cost, "modelUsage": {
            "claude-sonnet-5-5": {"inputTokens": input_tokens, "outputTokens": 50 * input_tokens // 1000,
                                  "cacheReadInputTokens": 900 * input_tokens // 1000,
                                  "cacheCreationInputTokens": 10, "thinkingTokens": 5, "costUSD": cost}}}

    # As Codex 0.163 reports it: `total` counts the thread so far.
    @staticmethod
    def codex_update(input_tokens):
        return {"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {
            "total": {"inputTokens": input_tokens, "cachedInputTokens": input_tokens * 9 // 10,
                      "outputTokens": 100, "reasoningOutputTokens": 40},
            "last": {"inputTokens": 1, "cachedInputTokens": 0, "outputTokens": 1, "reasoningOutputTokens": 0}}}}

    def test_claude_usage_comes_from_the_result_events_model_usage(self):
        totals = run.claude_totals([{"type": "assistant"}, self.claude_result(1000, 0.0026)])
        self.assertEqual(totals, {"claude-sonnet-5-5": {
            "input": 1910, "cached_input": 900, "output": 50, "reasoning": 5, "cost_usd": 0.0026}})
        self.assertEqual(run.claude_totals([{"type": "assistant"}]), {})

    def test_codex_usage_comes_from_the_last_token_usage_update(self):
        events = [self.codex_update(500), {"method": "item/completed"}, self.codex_update(2000)]
        totals = run.codex_totals(events, "gpt-6-luna")
        used = totals["gpt-6-luna"]
        self.assertEqual({k: v for k, v in used.items() if k != "cost_usd"},
                         {"input": 2000, "cached_input": 1800, "output": 100, "reasoning": 40})
        # 200 uncached at $0.10, 1,800 cached at $0.01, 100 out at $0.50 per 1M.
        self.assertAlmostEqual(used["cost_usd"], (200 * 0.10 + 1800 * 0.01 + 100 * 0.50) / 1e6)
        self.assertIsNone(run.codex_totals(events, "gpt-6.1-sol")["gpt-6.1-sol"]["cost_usd"])
        self.assertEqual(run.codex_totals([{"method": "turn/completed"}], "gpt-6-luna"), {})

    def test_a_step_counts_only_what_its_turns_used(self):
        # Two turns in one conversation, the second after a resume: both
        # clients carry the running totals on, so each step is the change.
        steps, totals = [], {}
        for events in ([self.claude_result(1000, 0.0026)], [self.claude_result(3000, 0.0078)]):
            now = run.claude_totals(events)
            steps.append(run.usage_since(now, totals))
            totals = now
        self.assertEqual([s["claude-sonnet-5-5"]["input"] for s in steps], [1910, 3800])
        self.assertAlmostEqual(steps[1]["claude-sonnet-5-5"]["cost_usd"], 0.0052)
        trial = {}
        for step in steps:
            run.add_usage(trial, step)
        self.assertEqual(trial["claude-sonnet-5-5"]["input"], 5710)
        self.assertAlmostEqual(trial["claude-sonnet-5-5"]["cost_usd"], 0.0078)
        # An unknown cost stays unknown in the sum.
        run.add_usage(trial, {"claude-sonnet-5-5": {**steps[0]["claude-sonnet-5-5"], "cost_usd": None}})
        self.assertIsNone(trial["claude-sonnet-5-5"]["cost_usd"])

    def test_the_report_shows_usage_per_scenario_client_and_model(self):
        used = {"input": 1000, "cached_input": 900, "output": 50, "reasoning": 0, "cost_usd": 0.5}
        table = run.usage_table([
            {"scenario": "s", "client": "claude", "binary": "a1", "usage": {"claude-sonnet-5-5": used}},
            {"scenario": "s", "client": "claude", "binary": "a1", "usage": {"claude-sonnet-5-5": used}},
            {"scenario": "s", "client": "codex", "usage": {"gpt-6-luna": {**used, "cost_usd": None}}},
            # A trial saved before usage was recorded.
            {"scenario": "t", "client": "codex"},
        ])
        self.assertIn("| s | claude | a1 | claude-sonnet-5-5 | 2,000 | 1,800 | 100 | 0 | $1.00 |", table)
        self.assertIn("| s | codex | - | gpt-6-luna | 1,000 | 900 | 50 | 0 | - |", table)
        self.assertIn("| all | | | | 3,000 | 2,700 | 150 | 0 | - |", table)


class BinariesUnderTest(unittest.TestCase):
    def binary(self, folder, name, contents):
        path = folder / name
        path.write_bytes(contents)
        path.chmod(0o755)
        return path

    def test_two_builds_with_the_same_contents_stop_the_run(self):
        folder = Path(tempfile.mkdtemp())
        base = self.binary(folder, "user-memory-67019a2", b"build of main")
        same = self.binary(folder, "user-memory-a8e9f58", b"build of main")
        other = self.binary(folder, "branch", b"build of the branch")
        binaries = run.under_test([f"main={base}", str(same)])
        self.assertEqual([b["label"] for b in binaries], ["main", "user-memory-a8e9f58"])
        self.assertEqual(binaries[0]["sha256"], binaries[1]["sha256"])
        stop = run.same_builds(binaries)
        self.assertIn("are the same file contents", stop)
        self.assertIn("tar -x -m", stop)
        self.assertIsNone(run.same_builds(run.under_test([f"main={base}", f"branch={other}"])))
        self.assertIsNone(run.same_builds(run.under_test([str(other)])))
        with self.assertRaises(SystemExit):
            run.under_test([f"x={base}", f"x={other}"])
        with self.assertRaises(SystemExit):
            run.under_test([str(folder / "missing")])

    def test_the_report_names_each_binary_and_its_hash(self):
        batch = Path(tempfile.mkdtemp())
        (batch / "binaries.json").write_text(json.dumps(
            [{"label": "main", "path": "/b/user-memory-67019a2", "sha256": "ab" * 32}]))
        text = run.summarize(batch, [{"scenario": "s", "client": "codex", "binary": "main", "passed": True,
                                      "steps": [], "usage": {}}])
        self.assertIn(f"- main: `/b/user-memory-67019a2` sha256 `{'ab' * 32}`", text)
        self.assertIn("| s | codex | main | 1/1 | - |", text)


class FakeModel(unittest.TestCase):
    """The fake model's own behaviour, with plain HTTP: no client needed."""

    @staticmethod
    def post(url, path, body, headers=()):
        request = urllib.request.Request(url + path, data=json.dumps(body).encode(),
                                         headers={"content-type": "application/json", **dict(headers)})
        with urllib.request.urlopen(request, timeout=10) as reply:
            return reply.read().decode()

    @staticmethod
    def events(stream):
        return [json.loads(line[len("data: "):]) for line in stream.splitlines() if line.startswith("data: ")]

    def test_it_listens_on_loopback_and_answers_from_its_script_in_order(self):
        log = Path(tempfile.mkdtemp()) / "model.jsonl"
        script = {"anthropic": [{"tool": "AskUserQuestion", "input": {"questions": []}}, {"text": "noted"}],
                  "openai": [{"tool": "update_plan", "input": {"plan": []}}]}
        with fake_model.FakeModel(script, log) as model:
            self.assertTrue(model.url.startswith("http://127.0.0.1:"))
            tools = [{"name": "x"}]
            first = self.events(self.post(model.url, "/v1/messages", {"stream": True, "tools": tools},
                                          [("authorization", "Bearer secret-token-123")]))
            block = next(e for e in first if e["type"] == "content_block_start")["content_block"]
            delta = next(e for e in first if e["type"] == "content_block_delta")["delta"]
            self.assertEqual((block["type"], block["name"]), ("tool_use", "AskUserQuestion"))
            self.assertEqual(json.loads(delta["partial_json"]), {"questions": []})
            # Claude Code's side calls carry no tools and take no reply.
            side = json.loads(self.post(model.url, "/v1/messages", {"messages": []}))
            self.assertEqual(side["content"], [{"type": "text", "text": "side"}])
            second = json.loads(self.post(model.url, "/v1/messages", {"tools": tools}))
            self.assertEqual(second["content"], [{"type": "text", "text": "noted"}])
            third = json.loads(self.post(model.url, "/v1/messages", {"tools": tools}))
            self.assertEqual(third["content"], [{"type": "text", "text": "done"}])
            openai = self.events(self.post(model.url, "/v1/responses", {"input": []}))
            item = next(e for e in openai if e["type"] == "response.output_item.done")["item"]
            self.assertEqual((item["type"], item["name"]), ("function_call", "update_plan"))
            usage = next(e for e in openai if e["type"] == "response.completed")["response"]["usage"]
            self.assertEqual((usage["input_tokens"], usage["input_tokens_details"]["cached_tokens"]),
                             (fake_model.USAGE["input"], fake_model.USAGE["cached_input"]))
            self.assertEqual(len(model.main_requests("anthropic")), 3)
        # Bodies are kept; headers, and the token in one, never are.
        logged = log.read_text()
        self.assertEqual(len(logged.splitlines()), 5)
        self.assertNotIn("secret-token-123", logged)
        self.assertNotIn("authorization", logged.lower())
        self.assertNotIn("secret-token-123", json.dumps(model.requests))

    def test_a_held_reply_waits_for_release(self):
        with fake_model.FakeModel({"openai": [{"text": "late", "hold": True}]}) as model:
            answered = []
            thread = threading.Thread(target=lambda: answered.append(
                self.post(model.url, "/v1/responses", {"input": []})))
            thread.start()
            self.assertTrue(model.wait_held(10))
            time.sleep(0.2)
            self.assertEqual(answered, [])
            model.release()
            thread.join(10)
            self.assertIn("late", answered[0])


class CommitsAndSeeds(unittest.TestCase):
    def grade(self, spec, messages):
        sandbox = types.SimpleNamespace(workdir=lambda spec, projects: Path("/x"),
                                        commit_messages=lambda path: messages)
        return run.grade({"commits": spec}, sandbox, [], [])[0]

    def test_commits_matches_a_subject(self):
        past = "^([\\w-]+(\\([\\w-]+\\))?!?: )?[a-z]+ed\\b"
        self.assertTrue(self.grade({"cwd": "project:n"}, ["Add x"]))
        self.assertFalse(self.grade({"cwd": "project:n"}, []))
        for subject in ["Added the review line", "docs: added the review line", "docs(readme)!: Added x"]:
            self.assertTrue(self.grade({"cwd": "project:n", "matches": past}, [subject]), subject)
        # The subject decides, not a line of the body.
        for message in ["Add the review line", "docs: add the line\n\nAdded because reviewers asked."]:
            self.assertFalse(self.grade({"cwd": "project:n", "matches": past}, [message]), message)

    def test_a_seed_can_name_an_earlier_seed(self):
        ids = {"old": "0f2c6a4e-1111-4222-8333-944455556666"}
        seed = {"id": "new", "args": ["--title", "t", "--supersedes", "{seed:old}"]}
        self.assertEqual(run.seed_args(seed, ids), ["--title", "t", "--supersedes", ids["old"]])
        with self.assertRaises(ValueError):
            run.seed_args({"id": "new", "args": ["--supersedes", "{seed:later}"]}, ids)

    def test_seeds_are_recorded_in_order_with_earlier_ids(self):
        recorded = []

        class Sandbox:
            def __init__(self, base, source, client):
                self.ids = {}

            def cli(self, *args):
                recorded.append(args)
                return json.dumps({"stored": True, "id": f"id-{len(recorded)}"})

            def setup(self, hooks):
                return ""
        original = run.Sandbox
        run.Sandbox = Sandbox
        try:
            scenario = {"seed": [{"id": "old", "args": ["--title", "a"]},
                                 {"id": "new", "args": ["--supersedes", "{seed:old}"]}]}
            sandbox = run.prepare(scenario, "claude", None, None, {})
        finally:
            run.Sandbox = original
        self.assertEqual(recorded, [("record", "--title", "a"), ("record", "--supersedes", "id-1")])
        self.assertEqual(sandbox.ids, {"old": "id-1", "new": "id-2"})


class Rehearsal(unittest.TestCase):
    def test_a_rehearse_entry_can_require_text_to_be_absent(self):
        trial = Path(tempfile.mkdtemp())
        sandbox = types.SimpleNamespace(binary=trial / "user-memory", env={}, db=trial / "db",
                                        workdir=lambda spec, projects: trial)
        header = "Rules\n  Write commit subjects in the past tense\n"
        original = run.subprocess.run
        run.subprocess.run = lambda *a, **k: types.SimpleNamespace(stdout=header)
        try:
            def notes(entry):
                return run.header_notes({"rehearse": [{"cwd": "plain", **entry}]}, "claude", sandbox, {})
            self.assertEqual(notes({"matches": ["past tense"], "lacks": ["imperative"]}), [])
            self.assertEqual(len(notes({"lacks": ["past tense"]})), 1)
            self.assertEqual(len(notes({"matches": ["imperative"]})), 1)
        finally:
            run.subprocess.run = original


if __name__ == "__main__":
    unittest.main()
