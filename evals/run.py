#!/usr/bin/env python3
"""Evaluate how real agents (Claude Code, Codex) use user memory.

Each trial installs the built `user-memory` binary into its own home with
`user-memory setup`, exactly as a user would (instruction files, hooks,
skill, MCP registration), seeds its own store, runs real agent sessions in
that home, and grades the final store, the agents' tool calls and commands,
their answers and the files they left.

Usage:
    evals/run.py [--client claude|codex|both] [--scenario NAME ...]
                 [--budget N] [--list] [--dry-run] [--rehearse]

Every agent session is a billed run. `--dry-run` prints the plan, the login
files and the guard without running anything; `--rehearse` also installs
every trial and checks what setup wrote, still without a single session.
`--budget` caps the total number of sessions recorded in the output
directory's run counter (default 100); a batch that would exceed it is
refused before it starts.

Isolation: each trial has its own HOME, CLAUDE_CONFIG_DIR, CODEX_HOME,
XDG_STATE_HOME, store and binary. The clients sign in with dedicated eval
logins the user created, never the user's own: Claude through a long-lived
token read from a 0600 file and passed only in the session's environment,
Codex through a symlink to a dedicated login, removed after each session.
Before the first session and after every session the guard checks that the
user's real memory, client configuration, instruction files and logins are
unchanged, and aborts otherwise.

Reports and raw transcripts go to $USER_MEMORY_EVAL_OUT (default
/tmp/user-memory-evals), never into the repository.
"""

import argparse
import hashlib
import json
import os
import queue
import re
import shlex
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOME = Path.home()
OUT = Path(os.environ.get("USER_MEMORY_EVAL_OUT", "/tmp/user-memory-evals"))

# The user's defaults (Claude: ~/.claude/settings.json "model"; Codex:
# ~/.codex/config.toml "model" and "model_reasoning_effort"). Trials never
# read those files, so the defaults are passed explicitly.
CLAUDE_MODEL = os.environ.get("USER_MEMORY_EVAL_CLAUDE_MODEL", "opus[1m]")
CODEX_MODEL = os.environ.get("USER_MEMORY_EVAL_CODEX_MODEL", "gpt-6.1-sol")
CODEX_EFFORT = os.environ.get("USER_MEMORY_EVAL_CODEX_EFFORT", "high")
# One agent turn; a turn that takes longer is cut off and graded as is.
TURN_TIMEOUT = 600
# Starting a Codex app-server and answering one request.
CODEX_REQUEST_TIMEOUT = 60

# Trials (homes, stores, binaries, projects) live here, one 0700 folder per
# batch: outside /tmp and TMPDIR, which Codex's workspace sandbox may write,
# so a sandboxed command cannot plant a program a trial later runs outside
# it. OUT holds only reports.
TRIALS = Path(os.environ.get("USER_MEMORY_EVAL_TRIALS",
                             HOME / "Library/Caches/user-memory-eval/trials"))
# The system git, never one found first on a trial's PATH.
GIT = "/usr/bin/git"

# Dedicated eval logins the user created for these runs, never their own.
EVAL_LOGINS = HOME / ".local/share/user-memory-eval"
CLAUDE_TOKEN_FILE = Path(os.environ.get("USER_MEMORY_EVAL_CLAUDE_TOKEN_FILE",
                                        EVAL_LOGINS / "claude-token"))
CODEX_AUTH_HOME = Path(os.environ.get("USER_MEMORY_EVAL_CODEX_AUTH", EVAL_LOGINS / "codex-auth"))

CODEX_CANDIDATES = [
    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    "/Applications/Codex.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
]


# ---------------------------------------------------------------- isolation guard

GUARDED = [
    HOME / ".local/share/user-memory/memories.sqlite3",
    HOME / ".local/share/user-memory/memories.sqlite3-wal",
    HOME / ".local/share/user-memory/memories.sqlite3-shm",
    HOME / ".local/bin/user-memory",
    HOME / ".local/bin/.user-memory.json",
    HOME / ".claude/settings.json",
    HOME / ".claude/settings.local.json",
    HOME / ".claude/CLAUDE.md",
    HOME / ".claude/rules/user-memory.md",
    HOME / ".codex/config.toml",
    HOME / ".codex/hooks.json",
    HOME / ".codex/AGENTS.md",
    HOME / ".codex/AGENTS.override.md",
    HOME / ".codex/auth.json",
]
# Other running Claude sessions rewrite caches and counters in ~/.claude.json
# continuously, so only its MCP server definitions are compared.
CLAUDE_STATE = HOME / ".claude.json"
STORE_DIR = HOME / ".local/share/user-memory"
# The user's own sessions write their snapshots here all the time; a trial
# session never may, so the guard looks for snapshots named after one.
REAL_SESSIONS = HOME / ".local/state/user-memory/sessions"
# Claude Code keeps a folder per working directory here, named after its
# path; one named after a trial would mean a session used the real config.
REAL_CLAUDE_PROJECTS = HOME / ".claude/projects"


def trial_projects():
    """Folders in the real ~/.claude/projects named after a trial path."""
    slug = re.sub(r"[^A-Za-z0-9]", "-", str(TRIALS.resolve())).strip("-")
    if not REAL_CLAUDE_PROJECTS.is_dir():
        return set()
    return {p.name for p in REAL_CLAUDE_PROJECTS.iterdir() if slug in p.name}


def fingerprint():
    state = {}
    # The Codex the trials run lives in the user's real ~/.codex; an update
    # during a batch would change it under the trials.
    codex = Path(codex_binary())
    state[f"{codex} -> {codex.resolve()}"] = hashlib.sha256(codex.resolve().read_bytes()).hexdigest()
    # No new file (another store, a journal, a lock) may appear next to the real store.
    names = sorted(p.name for p in STORE_DIR.iterdir()) if STORE_DIR.exists() else []
    state[f"{STORE_DIR}/ (entries)"] = ",".join(names) or "empty"
    for path in GUARDED:
        state[str(path)] = (
            hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else "absent"
        )
    if CLAUDE_STATE.exists():
        data = json.loads(CLAUDE_STATE.read_text())
        servers = {
            "mcpServers": data.get("mcpServers"),
            "projects": {
                name: project.get("mcpServers")
                for name, project in sorted((data.get("projects") or {}).items())
                if project.get("mcpServers")
            },
        }
        digest = hashlib.sha256(json.dumps(servers, sort_keys=True).encode()).hexdigest()
        state[f"{CLAUDE_STATE} (mcpServers)"] = digest
    return state


class Guard:
    def __init__(self, log):
        self.baseline = fingerprint()
        self.projects = trial_projects()
        self.log = log
        log.write("guard baseline:\n")
        for path, digest in self.baseline.items():
            log.write(f"  {digest[:16]}  {path}\n")
        log.write(f"  plus: no snapshot under {REAL_SESSIONS} named after a trial session\n")
        log.write(f"  plus: no new folder under {REAL_CLAUDE_PROJECTS} named after a trial path\n")
        log.flush()

    def check(self, after, conversation_ids=()):
        now = fingerprint()
        changed = [p for p in self.baseline if self.baseline[p] != now.get(p)]
        leaked = sorted(
            str(path) for cid in conversation_ids if cid
            for path in (REAL_SESSIONS.glob(f"*-{cid}.json") if REAL_SESSIONS.is_dir() else [])
        )
        projects = sorted(str(REAL_CLAUDE_PROJECTS / name)
                          for name in trial_projects() - self.projects)
        problems = changed + leaked + projects
        self.log.write(f"guard after {after}: {'CHANGED ' + ', '.join(problems) if problems else 'unchanged'}\n")
        self.log.flush()
        if problems:
            sys.exit(f"ABORT: real user state changed after {after}: {', '.join(problems)}")


# ---------------------------------------------------------------- logins

def claude_token_problem():
    """Why the Claude eval token cannot be used, or None. Reads only its mode."""
    if not CLAUDE_TOKEN_FILE.is_file():
        return f"missing {CLAUDE_TOKEN_FILE} (run `claude setup-token` and save the token there)"
    mode = stat.S_IMODE(CLAUDE_TOKEN_FILE.stat().st_mode)
    if mode & 0o077:
        return f"{CLAUDE_TOKEN_FILE} has mode {mode:o}; it must be 0600 or narrower"
    return None


def codex_login_problem():
    """Why the Codex eval login cannot be used, or None. Reads nothing in it."""
    if not (CODEX_AUTH_HOME / "auth.json").is_file():
        return (f"missing {CODEX_AUTH_HOME / 'auth.json'} "
                f"(run `CODEX_HOME={CODEX_AUTH_HOME} codex login`)")
    return None


def claude_token():
    """The eval token, for a session's environment only: never printed,
    logged or written to a report."""
    return CLAUDE_TOKEN_FILE.read_text().strip()


# ---------------------------------------------------------------- run budget

def counted_runs():
    try:
        return int((OUT / "run-count").read_text())
    except FileNotFoundError:
        return 0


def count_run():
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "run-count").write_text(str(counted_runs() + 1))


# ---------------------------------------------------------------- sandbox

def claude_binary():
    found = shutil.which("claude")
    if not found:
        sys.exit("Claude Code CLI not found")
    return Path(found).resolve()


def codex_binary():
    """The Codex CLI on PATH, else one inside an installed app, resolved once
    so a batch runs one version throughout."""
    global CODEX
    if CODEX is None:
        found = shutil.which("codex") or next(
            (c for c in CODEX_CANDIDATES if os.access(c, os.X_OK)), None)
        if not found:
            sys.exit("Codex CLI not found")
        CODEX = found
    return CODEX


CODEX = None


class Sandbox:
    """One trial's own home, client configuration, state, store and binary,
    installed by `user-memory setup`."""

    def __init__(self, base, source, client):
        self.client = client
        # Resolved, as /tmp is a symlink on macOS: an allow rule for a path
        # applies only when the path and what it resolves to both match.
        self.dir = Path(tempfile.mkdtemp(prefix=f"trial-{client}-", dir=base)).resolve()
        self.home = self.dir / "home"
        self.claude_dir = self.home / ".claude"
        self.codex_home = self.home / ".codex"
        self.state = self.dir / "state"
        self.install = self.dir / "bin"
        self.tools = self.dir / "tools"
        self.db = self.dir / "memory.sqlite3"
        for path in (self.home, self.claude_dir, self.codex_home, self.state, self.tools):
            path.mkdir(parents=True)
        # Only the client's own CLI on PATH, never the folder holding the
        # user's installed user-memory.
        if client == "claude":
            (self.tools / "claude").symlink_to(claude_binary())
        else:
            (self.tools / "codex").symlink_to(Path(codex_binary()).resolve())
        self.source = source
        self.binary = self.install / "user-memory"
        self.setup_output = ""
        self.env = {
            "HOME": str(self.home),
            "USER": os.environ.get("USER", ""),
            "LANG": os.environ.get("LANG", "en_US.UTF-8"),
            "TMPDIR": tempfile.gettempdir() + "/",
            # System tools first, so `git` is always the system one.
            "PATH": f"/usr/bin:/bin:/usr/sbin:/sbin:{self.install}:{self.tools}",
            "XDG_STATE_HOME": str(self.state),
            "CLAUDE_CONFIG_DIR": str(self.claude_dir),
            "CODEX_HOME": str(self.codex_home),
            "USER_MEMORY_DB": str(self.db),
        }
        self.ids = {}

    def cli(self, *args):
        binary = self.binary if self.binary.exists() else self.source
        out = subprocess.run(
            [str(binary), *args], env=self.env, capture_output=True, text=True, timeout=60
        )
        if out.returncode != 0:
            raise RuntimeError(f"user-memory {' '.join(args)} failed: {out.stderr.strip()}")
        return out.stdout

    def setup(self, hooks):
        """Installs user-memory for this trial's client, as a user would."""
        args = ["setup", "--client", self.client, "--install-dir", str(self.install),
                "--db", str(self.db)]
        if not hooks:
            args.append("--no-hooks")
        out = subprocess.run([str(self.source), *args], env=self.env, capture_output=True,
                             text=True, timeout=180)
        if out.returncode != 0:
            raise RuntimeError(f"setup failed: {out.stderr.strip() or out.stdout.strip()}")
        if self.client == "codex" and hooks:
            approve_codex_hooks(self)
        elif self.client == "codex":
            # Codex creates its own state, its memory databases among them,
            # when it first starts; start it once so a trial's baseline
            # holds them.
            user_memory_hooks(self)
        return out.stdout

    def instruction_file(self):
        if self.client == "claude":
            return self.claude_dir / "rules/user-memory.md"
        return self.codex_home / "AGENTS.md"

    def workdir(self, spec, projects):
        """The trial folder `spec` names, created on first use: `plain`, a
        `folder:PATH` without git, or a `project:PATH` checkout whose remote
        is the project named by PATH's last part, so `project:space/app`
        is a checkout of `app` inside the folder `space`."""
        if spec == "plain":
            path = self.dir / "work"
            path.mkdir(exist_ok=True)
            return path
        if spec.startswith("folder:"):
            path = self.dir / spec.removeprefix("folder:")
            path.mkdir(parents=True, exist_ok=True)
            return path
        name = spec.removeprefix("project:")
        path = self.dir / name
        if not path.exists():
            path.mkdir(parents=True)
            git = lambda *a: subprocess.run(  # noqa: E731
                [GIT, *a], cwd=path, env={**self.env, "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": "/dev/null"}, check=True, capture_output=True)
            git("init", "-q", "-b", "main")
            git("config", "user.name", "Eval User")
            git("config", "user.email", "eval@example.invalid")
            (path / "README.md").write_text(f"# {name}\n")
            git("add", "README.md")
            git("commit", "-q", "-m", "Start the project")
            git("remote", "add", "origin", projects[Path(name).name])
            # A push can never leave the machine: it goes to a path that
            # does not exist.
            git("remote", "set-url", "--push", "origin", str(self.dir / "no-push-remote"))
        return path

    def commit_messages(self, path):
        """Messages of the commits after the project's first one."""
        if not (path / ".git").exists():
            return []
        out = subprocess.run([GIT, "log", "--format=%B%x00", "main"], cwd=path, env={
            **self.env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"},
            capture_output=True, text=True)
        messages = [m.strip() for m in out.stdout.split("\0") if m.strip()]
        return messages[:-1]

    def memories(self):
        return json.loads(self.cli("search", "--scope", "all", "--status", "all", "--page-size", "200"))["memories"]


def reader(stream):
    """A queue of `stream`'s lines, then None at its end."""
    lines = queue.Queue()

    def pump():
        for line in stream:
            lines.put(line)
        lines.put(None)
    threading.Thread(target=pump, daemon=True).start()
    return lines


def app_server_request(env, requests, wanted_id):
    """Sends `requests` to a short-lived `codex app-server` and returns the
    reply with id `wanted_id`."""
    child = subprocess.Popen([codex_binary(), "app-server"], env=env, cwd=env["HOME"],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
    try:
        for request in requests:
            child.stdin.write(json.dumps(request) + "\n")
        child.stdin.flush()
        lines = reader(child.stdout)
        deadline = time.time() + CODEX_REQUEST_TIMEOUT
        while True:
            line = lines.get(timeout=max(0.1, deadline - time.time()))
            if line is None:
                raise RuntimeError("codex app-server closed before answering")
            message = json.loads(line)
            if message.get("id") == wanted_id:
                return message
    finally:
        child.kill()
        child.wait()


def user_memory_hooks(sandbox):
    """The trial's user-memory hooks as Codex lists them."""
    reply = app_server_request(sandbox.env, [
        {"id": 0, "method": "initialize", "params": {"clientInfo": {"name": "eval", "version": "1"}}},
        {"method": "initialized"},
        {"id": 1, "method": "hooks/list", "params": {"cwds": [str(sandbox.dir)]}},
    ], 1)
    return [hook for entry in reply["result"]["data"] for hook in entry["hooks"]
            if "user-memory" in hook.get("command", "")]


def approve_codex_hooks(sandbox):
    """Approves the trial's user-memory hooks the way a user does in /hooks:
    Codex records each hook's current hash as trusted in config.toml."""
    hooks = user_memory_hooks(sandbox)
    if len(hooks) != 2:
        raise RuntimeError(f"Codex lists {len(hooks)} user-memory hooks, not 2")
    config = sandbox.codex_home / "config.toml"
    text = config.read_text() if config.exists() else ""
    for hook in hooks:
        text += f"\n[hooks.state.{json.dumps(hook['key'])}]\ntrusted_hash = {json.dumps(hook['currentHash'])}\n"
    config.write_text(text)


# ---------------------------------------------------------------- sessions

# File tools a scenario may give Claude so its own file-based auto memory can
# compete with user-memory. Writes stay unapproved: a write is refused and
# the attempt shows in the event stream.
CLAUDE_FILE_TOOLS = "Read,Write,Edit"
WRITE_TOOLS = {"Write", "Edit", "MultiEdit", "NotebookEdit"}
# A shell scenario's Claude may edit files in its trial and run git status,
# add and commit there. Reads outside the working directory and every other
# command stay unapproved, and so refused.
CLAUDE_SHELL_TOOLS = "Bash,Read,Edit,Write"


def claude_shell_rules(trial, project, files):
    """(allow, deny) permission rules for a shell scenario's Claude working
    in `project`, a resolved folder of `trial` whose tracked `files` it may
    stage. `//` anchors an Edit rule at the filesystem root, and Edit rules
    cover Write. Bash rules match each part of a compound command; the allow
    rules are exact shapes, so no option can slip in. A commit takes its
    message from -m: git refuses -F next to it and ignores --template. The
    deny rules are a backstop, and keep edits out of the trial's own
    programs, home and state."""
    # As agents run them: in the project, or pointed at it with -C.
    gits = ["git", f"git -C {project}"]
    allow = [f"Edit(/{project}/**)", "Bash(git status)", "Bash(git status --short)",
             *[f"Bash({git} add {what})" for git in gits for what in [".", "-A", *files]],
             *[f"Bash({git} commit {shape})" for git in gits
               for shape in COMMIT_SHAPES]]
    deny = ["Bash(git push *)", "Bash(git * --output*)", "Bash(git add *--pathspec*)",
            *[f"Bash({git} commit *{option}*)" for git in gits for option in COMMIT_DENIED],
            *[f"Edit(/{trial}/{folder}/**)" for folder in ("bin", "home", "tools", "state")]]
    return allow, deny


# How agents give a commit its message: always with -m, quietly or with -a
# for the tracked changes.
COMMIT_SHAPES = ["-m *", "-q -m *", "-qm *", "-am *", "-a -m *", "-q -am *", "-qam *"]
# What no allowed commit may carry: a message from a file, a template, or a
# rewrite of the last commit.
COMMIT_DENIED = ["-F", "--fi", "--t", "-t ", "--amend"]


def drain(lines):
    text = []
    while True:
        try:
            line = lines.get_nowait()
        except queue.Empty:
            break
        if line is None:
            break
        text.append(line)
    return "".join(text)[-4000:]


class ClaudeSession:
    """A Claude Code session that stays open between turns: each turn is a
    user message on stdin, answered by a `result` event."""

    def __init__(self, sandbox, cwd, options, resume=None):
        tools, allowed, denied = "", ["mcp__user-memory"], []
        if options.get("commands") and not options.get("shell"):
            # Only the user-memory commands a scenario names, by name or by
            # the trial's installed path.
            tools = "Bash"
            for command in options["commands"]:
                rest = command.removeprefix("user-memory ")
                allowed += [f"Bash({command} *)", f"Bash({sandbox.binary} {rest} *)"]
        if options.get("shell"):
            files = subprocess.run([GIT, "ls-files"], cwd=cwd, capture_output=True,
                                   text=True).stdout.split()
            allow, denied = claude_shell_rules(sandbox.dir, Path(cwd).resolve(), files)
            tools, allowed = CLAUDE_SHELL_TOOLS, allowed + allow
        elif options.get("claude_file_tools"):
            tools = CLAUDE_FILE_TOOLS
        self.command = [
            "claude", "-p", "--input-format", "stream-json",
            "--output-format", "stream-json", "--verbose",
            "--model", CLAUDE_MODEL,
            # Only the trial's own user settings, written by setup: its
            # hooks, its rules and its MCP registration.
            "--setting-sources", "user",
            "--tools", tools,
            "--allowedTools", ",".join(allowed),
            *(["--disallowedTools", ",".join(denied)] if denied else []),
            # A scenario that compacts needs /compact; one that resumes keeps
            # its conversation in the trial's own CLAUDE_CONFIG_DIR.
            *([] if options.get("compacts") else ["--disable-slash-commands"]),
            *([] if options.get("resumes") else ["--no-session-persistence"]),
            *(["--resume", resume] if resume else []),
        ]
        if options.get("claude_file_tools"):
            self.command += ["--settings", json.dumps({"autoMemoryEnabled": True})]
        # No self-update of the user's installed Claude Code mid-batch.
        # The token signs the session in; the scrub keeps it out of every
        # command, hook and MCP server Claude starts, so no command can put it
        # in a commit or a result.
        env = {**sandbox.env, "CLAUDE_CODE_OAUTH_TOKEN": claude_token(),
               "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB": "1", "DISABLE_AUTOUPDATER": "1"}
        self.child = subprocess.Popen(self.command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.lines = reader(self.child.stdout)
        self.errors = reader(self.child.stderr)

    def turn(self, prompt):
        message = {"type": "user", "message": {"role": "user", "content": prompt}}
        events, error = [], None
        try:
            self.child.stdin.write(json.dumps(message) + "\n")
            self.child.stdin.flush()
        except BrokenPipeError:
            return events, "the session had ended"
        deadline = time.time() + TURN_TIMEOUT
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                error = f"timed out after {TURN_TIMEOUT}s"
                break
            if line is None:
                error = "the session ended"
                break
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            events.append(event)
            if event.get("type") == "result":
                break
        return events, error

    def compact(self):
        """Compacts the conversation as a user's /compact does."""
        return self.turn("/compact")

    def close(self):
        try:
            self.child.stdin.close()
            self.child.wait(timeout=30)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            self.child.kill()
            self.child.wait()
        return drain(self.errors)


def toml_string(value):
    return json.dumps(str(value))


CODEX_EXTRA = []


class CodexSession:
    """A Codex session through `codex app-server`: one thread, one turn per
    prompt. Its configuration is the trial's CODEX_HOME, as setup wrote it."""

    def __init__(self, sandbox, cwd, options, resume=None):
        self.sandbox = sandbox
        self.resume = resume
        self.child = None
        link = sandbox.codex_home / "auth.json"
        link.unlink(missing_ok=True)
        link.symlink_to(CODEX_AUTH_HOME / "auth.json")
        try:
            self.start(cwd, options)
        except BaseException:
            # A session that never started keeps neither a process nor the login.
            self.close()
            raise

    def start(self, cwd, options):
        server = "mcp_servers.user-memory"
        shell = bool(options.get("shell") or options.get("commands"))
        # With escalations on, Codex may ask to run a command outside its
        # sandbox; `escalation` decides, and every request is logged.
        policy = "on-request" if options.get("codex_escalations") else "never"
        self.approvals = []
        self.command = [
            codex_binary(), "app-server",
            "--disable", "plugins", "-c", "web_search=\"disabled\"",
            "-c", f"model={toml_string(CODEX_MODEL)}",
            "-c", f"model_reasoning_effort={toml_string(CODEX_EFFORT)}",
            "-c", f"approval_policy={toml_string(policy)}",
            # The memory tools run without approval prompts, as for a user
            # who approved them for good.
            "-c", f"{server}.default_tools_approval_mode=\"approve\"",
            *(["--disable", "shell_tool"] if not shell else []),
            *[arg for setting in CODEX_EXTRA for arg in ("-c", setting)],
        ]
        self.child = subprocess.Popen(self.command, cwd=cwd, env=self.sandbox.env,
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, text=True)
        self.lines = reader(self.child.stdout)
        self.errors = reader(self.child.stderr)
        self.next_id = 10
        self.request("initialize", {"clientInfo": {"name": "user-memory-eval", "version": "1"}})
        self.send({"method": "initialized"})
        thread = {"cwd": str(cwd), "approvalPolicy": policy,
                  "sandbox": "workspace-write" if shell else "read-only", "model": CODEX_MODEL}
        if self.resume:
            started = self.request("thread/resume", {**thread, "threadId": self.resume})
        else:
            # A thread a scenario resumes is kept in the trial's CODEX_HOME.
            started = self.request("thread/start", {
                **thread, "ephemeral": not options.get("resumes"),
            })
        result = started.get("result") or {}
        self.thread = (result.get("thread") or {}).get("id")
        self.instruction_sources = result.get("instructionSources", [])
        if not self.thread:
            raise RuntimeError(f"codex thread/start failed: {started}")

    def send(self, message):
        self.child.stdin.write(json.dumps(message) + "\n")
        self.child.stdin.flush()

    def request(self, method, params, events=None):
        request_id = self.next_id
        self.next_id += 1
        self.send({"id": request_id, "method": method, "params": params})
        deadline = time.time() + CODEX_REQUEST_TIMEOUT
        while True:
            line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            if line is None:
                raise RuntimeError(f"codex app-server ended during {method}")
            message = json.loads(line)
            if message.get("id") == request_id and "method" not in message:
                return message
            self.answer(message)
            if events is not None:
                events.append(message)

    def answer(self, message):
        """Answers a request the server makes of the client: a command
        escalation is approved only as `escalation` allows, and anything
        else is declined. Each request and its decision is logged."""
        if "id" not in message or "method" not in message:
            return
        method, params = message["method"], message.get("params") or {}
        if method in ("item/commandExecution/requestApproval", "execCommandApproval"):
            command = params.get("command")
            command = " ".join(command) if isinstance(command, list) else str(command or "")
            approved = escalation(command, params.get("cwd"), self.sandbox.dir)
            decision = ("accept" if approved else "decline") if method.startswith("item/") \
                else ("approved" if approved else "denied")
            self.approvals.append({"method": method, "command": command,
                                   "cwd": params.get("cwd"), "decision": decision})
            self.send({"id": message["id"], "result": {"decision": decision}})
            return
        if method in ("item/fileChange/requestApproval", "applyPatchApproval"):
            decision = "decline" if method.startswith("item/") else "denied"
            self.approvals.append({"method": method, "command": "", "cwd": params.get("cwd"),
                                   "decision": decision})
            self.send({"id": message["id"], "result": {"decision": decision}})
            return
        self.approvals.append({"method": method, "command": "", "cwd": params.get("cwd"),
                               "decision": "declined (error reply)"})
        self.send({"id": message["id"], "error": {"code": -32601,
                                                  "message": "the eval client declines"}})

    def compact(self):
        """Compacts the thread and waits until Codex reports it compacted."""
        events, error = [], None
        reply = self.request("thread/compact/start", {"threadId": self.thread}, events)
        if "error" in reply:
            return events, f"thread/compact/start failed: {reply['error']}"
        deadline = time.time() + TURN_TIMEOUT
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                return events, f"timed out after {TURN_TIMEOUT}s"
            if line is None:
                return events, "codex app-server ended"
            message = json.loads(line)
            self.answer(message)
            events.append(message)
            if message.get("method") in ("thread/compacted", "turn/completed"):
                return events, None

    def turn(self, prompt):
        events, error = [], None
        self.approvals = []
        reply = self.request("turn/start", {"threadId": self.thread,
                                            "input": [{"type": "text", "text": prompt}]}, events)
        if "error" in reply:
            return events, f"turn/start failed: {reply['error']}"
        deadline = time.time() + TURN_TIMEOUT
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                error = f"timed out after {TURN_TIMEOUT}s"
                break
            if line is None:
                error = "codex app-server ended"
                break
            message = json.loads(line)
            self.answer(message)
            events.append(message)
            if message.get("method") == "turn/completed":
                break
        return events, error

    def close(self):
        try:
            if self.child is None:
                return ""
            self.child.kill()
            self.child.wait()
            return drain(self.errors)
        finally:
            # Once Codex has stopped, whatever it left there: the trial
            # keeps no login, even one a token refresh rewrote.
            (self.sandbox.codex_home / "auth.json").unlink(missing_ok=True)


# Options each git step may carry when Codex asks to run it outside its
# sandbox. Anything else, such as --output, -F, --file, --template or
# --pathspec-from-file, could write or read a file outside the trial.
STEP_OPTIONS = {
    "status": {"--short", "-s", "--porcelain", "-b", "--branch"},
    "diff": {"--stat", "--cached", "--staged", "--name-only", "--check"},
    "log": {"--oneline", "--stat"},
    "show": {"--stat", "--name-only", "--oneline"},
    "add": {"-A", "--all", "-u", "--update"},
    "commit": set(),
}
# Steps that change the repository; an approved command holds at least one.
WRITES = {"add", "commit"}
SHELLS = {"sh", "bash", "zsh"}
OPERATORS = set("();<>|&")


def escalation(command, cwd, trial):
    """Whether to let Codex run `command` outside its sandbox: git steps
    joined by && or ;, each with only the options STEP_OPTIONS allows, file
    paths inside the trial, at least one add or commit, and a commit message
    given only with -m or --message; run in a directory of the trial."""
    trial = Path(trial).resolve()
    if cwd is None or not Path(str(cwd)).resolve().is_relative_to(trial):
        return False
    # Shell expansion could put anything into an argument: variables,
    # substitutions, globs, braces, ~ and zsh's =command.
    if any(c in command for c in "$`~{}*?[]!") or re.search(r"(?:^|\s)=", command):
        return False
    try:
        outer = shlex.split(command)
        script = outer[2] if (len(outer) == 3 and Path(outer[0]).name in SHELLS
                              and outer[1] in ("-c", "-lc")) else command
        lexer = shlex.shlex(script, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return False
    steps, step = [], []
    for token in tokens + [";"]:
        if token in ("&&", ";"):
            if step:
                steps.append(step)
            step = []
        elif set(token) <= OPERATORS:
            return False
        else:
            step.append(token)
    here = Path(str(cwd)).resolve()
    return bool(steps) and all(git_step(s, here, trial) for s in steps) \
        and any(s[1] in WRITES for s in steps)


def git_step(tokens, cwd, trial):
    """Whether one git step stays within its allowlist."""
    if len(tokens) < 2 or tokens[0] != "git" or tokens[1] not in STEP_OPTIONS:
        return False
    sub, args = tokens[1], tokens[2:]
    allowed = STEP_OPTIONS[sub]
    i = 0
    while i < len(args):
        arg = args[i]
        if sub == "commit":
            if arg in ("-m", "--message") and i + 1 < len(args):
                i += 2
                continue
            if arg.startswith("--message="):
                i += 1
                continue
            return False
        if arg in allowed or arg == "--":
            pass
        elif sub in ("log", "show") and arg.startswith("--format="):
            pass
        elif sub == "log" and re.fullmatch(r"-\d+", arg):
            pass
        elif sub == "log" and arg == "-n" and i + 1 < len(args) and args[i + 1].isdigit():
            i += 1
        elif sub == "show" and re.fullmatch(r"HEAD(~\d+|\^)?|[0-9a-f]{7,40}", arg):
            pass
        elif sub in ("add", "diff") and not arg.startswith("-"):
            if not (cwd / arg).resolve().is_relative_to(trial):
                return False
        else:
            return False
        i += 1
    return True


def items(events):
    """Completed Codex items of a turn."""
    return [e["params"]["item"] for e in events
            if e.get("method") == "item/completed" and isinstance(e.get("params"), dict)]


def tool_calls(client, events):
    """(tool, arguments) for every memory tool call in a turn."""
    calls = []
    if client == "claude":
        for event in events:
            if event.get("type") == "assistant":
                for part in event.get("message", {}).get("content", []):
                    if part.get("type") == "tool_use" and part.get("name", "").startswith("mcp__user-memory__"):
                        calls.append((part["name"].removeprefix("mcp__user-memory__"), part.get("input", {})))
        return calls
    for item in items(events):
        if item.get("type") == "mcpToolCall":
            arguments = item.get("arguments")
            if isinstance(arguments, str):
                try:
                    arguments = json.loads(arguments)
                except json.JSONDecodeError:
                    arguments = {"raw": arguments}
            calls.append((item.get("tool", ""), arguments or {}))
    return calls


def shell_commands(client, events):
    """Every shell command a turn ran or tried to run."""
    if client == "claude":
        return [str(part.get("input", {}).get("command", ""))
                for event in events if event.get("type") == "assistant"
                for part in event.get("message", {}).get("content", [])
                if part.get("type") == "tool_use" and part.get("name") == "Bash"]
    return [str(item.get("command", "")) for item in items(events)
            if item.get("type") == "commandExecution"]


def file_writes(client, events):
    """Paths a Claude turn tried to write with its built-in file tools."""
    paths = []
    for event in events:
        if client == "claude" and event.get("type") == "assistant":
            for part in event.get("message", {}).get("content", []):
                if part.get("type") == "tool_use" and part.get("name") in WRITE_TOOLS:
                    params = part.get("input", {})
                    paths.append(str(params.get("file_path") or params.get("notebook_path") or ""))
    return paths


def codex_memory_rows(sandbox):
    """Rows per table in Codex's own memory databases in the trial's
    CODEX_HOME, read only."""
    rows = {}
    for path in sorted(sandbox.codex_home.glob("memories_*.sqlite")):
        conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        try:
            tables = [t for (t,) in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
            for table in tables:
                rows[f"{path.name}:{table}"] = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
        finally:
            conn.close()
    return rows


def client_memory_files(sandbox):
    """Files Claude Code's auto memory left in the trial's own projects folder."""
    projects = sandbox.claude_dir / "projects"
    if not projects.is_dir():
        return []
    return sorted(str(path) for path in projects.rglob("*")
                  if path.is_file() and "memory" in path.parts and not path.name.endswith(".jsonl"))


def session_ids(client, events):
    """Identifiers the client itself gives this conversation."""
    if client == "claude":
        return {e["session_id"] for e in events if e.get("session_id")}
    return {e["params"]["threadId"] for e in events
            if isinstance(e.get("params"), dict) and e["params"].get("threadId")}


def hook_context(client, events):
    """What hooks added to the conversation during a Codex turn: the context
    entries of each hook run Codex reports."""
    if client != "codex":
        return []
    return [entry.get("text", "") for e in events
            if e.get("method") == "hook/completed" and isinstance(e.get("params"), dict)
            for entry in (e["params"].get("run") or {}).get("entries") or []
            if entry.get("kind") == "context"]


def origins(record):
    """Each origin the agent recorded, and whether it carries the client's real
    conversation id."""
    found = []
    for name, args in record["tool_calls"]:
        if name == "record":
            origin = str(args.get("origin") or "")
            real = bool(origin) and any(i in origin for i in record["conversation_ids"])
            found.append({"tool": name, "origin": origin, "real": real})
    return found


def final_answer(client, events):
    if client == "claude":
        for event in reversed(events):
            if event.get("type") == "result":
                return event.get("result") or ""
        return ""
    texts = [item.get("text", "") for item in items(events) if item.get("type") == "agentMessage"]
    return texts[-1] if texts else ""


def open_session(client, sandbox, cwd, options, resume=None):
    count_run()
    if client == "claude":
        return ClaudeSession(sandbox, cwd, options, resume)
    return CodexSession(sandbox, cwd, options, resume)


# ---------------------------------------------------------------- scenarios

def steps(scenario):
    """The scenario's turns in order: each names its session (a process kept
    open until its last turn), working directory and prompt."""
    if "steps" in scenario:
        return scenario["steps"]
    return [{"session": f"s{i}", **session} for i, session in enumerate(scenario["sessions"])]


def session_names(scenario):
    return list(dict.fromkeys(step["session"] for step in steps(scenario)))


def session_opens(scenario):
    """Client processes a trial starts: one per session, and one more for
    each resume, which reopens a conversation in a new process."""
    return len(session_names(scenario)) + sum(1 for step in steps(scenario) if step.get("resume"))


def runs_of(scenario, clients):
    """(client, repeat) pairs a batch runs for `scenario`."""
    names = [c for c in clients if c in scenario.get("clients", ["claude", "codex"])]
    return [(c, r) for r in range(scenario.get("repeat", 1)) for c in names]


# ---------------------------------------------------------------- grading

def matches(memory, where):
    for field, pattern in where.items():
        value = memory.get(field)
        if value is None:
            return False
        if not re.search(pattern, str(value), re.IGNORECASE):
            return False
    return True


def resolve(value, sandbox, memories):
    """Replaces `{seed:NAME}` with a seeded memory's short id and
    `{memory:REGEX}` with the short id of the memory whose title or content
    matches, in every string of a check."""
    if isinstance(value, dict):
        return {k: resolve(v, sandbox, memories) for k, v in value.items()}
    if isinstance(value, list):
        return [resolve(v, sandbox, memories) for v in value]
    if not isinstance(value, str):
        return value

    def seed(match):
        return sandbox.ids[match.group(1)][:8]

    def memory(match):
        found = next((m for m in memories if re.search(match.group(1), m["title"] + " " + m["content"],
                                                       re.IGNORECASE)), None)
        return found["id"][:8] if found else "no-such-memory"
    value = re.sub(r"\{seed:([^}]+)\}", seed, value)
    return re.sub(r"\{memory:([^}]+)\}", memory, value)


def grade(check, sandbox, records, memories):
    """(passed, detail) for one check."""
    kind, spec = next(iter(check.items()))
    active = [m for m in memories if m["status"] in ("confirmed", "tentative")]

    def step():
        return records[spec.get("step", -1) if isinstance(spec, dict) else -1]
    if kind == "memory":
        pool = memories if spec.get("status") == "all" else active
        found = [m for m in pool if matches(m, spec.get("where", {}))]
        want = spec.get("count", ">=1")
        ok = len(found) >= int(want[2:]) if str(want).startswith(">=") else len(found) == int(want)
        if not ok:
            return False, f"{len(found)} memories match {spec.get('where')} (want {want})"
        for m in found:
            for field, rule in spec.get("fields", {}).items():
                value = m.get(field)
                good = (value not in ("", None)) if rule == "nonempty" else bool(
                    re.search(rule, str(value or ""), re.IGNORECASE))
                if not good:
                    return False, f"{field}={value!r} fails {rule!r} on {m['content']!r}"
        return True, f"{len(found)} match"
    if kind == "seed":
        memory = next((m for m in memories if m["id"] == sandbox.ids[spec["id"]]), None)
        if spec.get("absent"):
            return memory is None, "absent" if memory is None else f"still present: {memory['status']}"
        if memory is None:
            return False, "missing"
        for field, rule in spec.get("fields", {}).items():
            if not re.search(rule, str(memory.get(field)), re.IGNORECASE):
                return False, f"{field}={memory.get(field)!r} fails {rule!r}"
        return True, memory["status"]
    if kind in ("answer", "answer_not"):
        text = step()["answer"]
        hit = re.search(spec["matches"], text, re.IGNORECASE | re.MULTILINE)
        ok = bool(hit) if kind == "answer" else not hit
        return ok, (hit.group(0) if hit else "no match") if ok else f"answer: {text[:300]!r}"
    if kind == "real_origin":
        real = [o["origin"] for o in step()["origins"] if o["real"]]
        return bool(real), f"origins {[o['origin'] for o in step()['origins']]}"
    if kind == "no_client_memory":
        # Writes Claude tried with its own file tools (refused, so they never
        # land), and anything that did land in the trial's auto memory folder.
        tried = [p for r in records for p in r.get("file_writes", [])]
        landed = client_memory_files(sandbox)
        return not tried and not landed, f"tried {tried}, landed {landed}"
    if kind in ("called", "not_called"):
        names = [name for name, args in step()["tool_calls"]
                 if name in spec["tools"] and matches(args, spec.get("args", {}))]
        ok = bool(names) if kind == "called" else not names
        return ok, f"calls {[(n, a) for n, a in step()['tool_calls']]}"
    if kind in ("commands", "commands_not"):
        pool = [c for r in records for c in r["commands"]] if spec.get("step") == "all" else step()["commands"]
        hits = [c for c in pool if re.search(spec["matches"], c, re.IGNORECASE)]
        ok = bool(hits) if kind == "commands" else not hits
        return ok, f"matching {hits[:3]}" if hits else f"commands {pool[:5]}"
    if kind == "approvals_not":
        # A request Codex made to run a command outside its sandbox, such as a
        # push, counts as attempted whatever the answer was.
        asked = [a for r in records for a in r.get("approvals", [])]
        hits = [a for a in asked if re.search(spec["matches"], a["command"], re.IGNORECASE)]
        return not hits, f"requests {[(a['command'][:80], a['decision']) for a in asked]}"
    if kind == "commits":
        messages = sandbox.commit_messages(sandbox.workdir(spec["cwd"], {}))
        return bool(messages), f"{len(messages)} new commits: {[m[:80] for m in messages]}"
    if kind == "commits_not":
        path = sandbox.workdir(spec["cwd"], {})
        messages = sandbox.commit_messages(path)
        hits = [m for m in messages if re.search(spec["matches"], m, re.IGNORECASE)]
        return not hits, f"{len(messages)} new commits; matching {hits[:2]}"
    if kind == "hook_context":
        text = "\n".join(step()["hook_context"])
        hit = re.search(spec["matches"], text, re.IGNORECASE)
        return bool(hit), hit.group(0) if hit else f"hook context: {text[:300]!r}"
    if kind == "file":
        # A trial file after a step (`"step": N`), or at the end.
        if "step" in spec:
            text = records[spec["step"]]["files"].get(spec["path"])
        else:
            path = sandbox.dir / spec["path"]
            text = path.read_text() if path.is_file() else None
        if spec.get("absent"):
            return text is None, "absent" if text is None else f"exists: {text[:200]!r}"
        if text is None:
            return False, f"no {spec['path']}"
        hit = re.search(spec["matches"], text, re.IGNORECASE | re.MULTILINE)
        return bool(hit), hit.group(0) if hit else f"{spec['path']}: {text[:300]!r}"
    if kind == "instructions":
        path = sandbox.instruction_file()
        text = path.read_text() if path.exists() else ""
        hit = re.search(spec["matches"], text, re.IGNORECASE)
        return bool(hit), hit.group(0) if hit else f"not in {path}"
    raise ValueError(f"unknown check {kind}")


# ---------------------------------------------------------------- driver

def prepare(scenario, client, base, source, projects):
    """A trial's sandbox with its seeds stored, user-memory set up and the
    scenario's folders in place."""
    sandbox = Sandbox(base, source, client)
    for seed in scenario.get("seed", []):
        saved = json.loads(sandbox.cli("record", *seed["args"]))
        if not saved.get("stored"):
            raise RuntimeError(f"seed {seed} refused: {saved}")
        sandbox.ids[seed["id"]] = saved["id"]
    sandbox.setup_output = sandbox.setup(scenario.get("hooks", True))
    for spec in scenario.get("folders", []):
        sandbox.workdir(spec, projects)
    return sandbox


def watched(scenario, sandbox):
    """The scenario's watched files, by trial path: contents, or None."""
    files = {}
    for name in scenario.get("watch", []):
        path = sandbox.dir / name
        files[name] = path.read_text() if path.is_file() else None
    return files


def trial(scenario, client, base, source, projects, guard):
    sandbox = prepare(scenario, client, base, source, projects)
    options = {k: scenario[k] for k in ("shell", "claude_file_tools", "codex_escalations",
                                        "commands")
               if k in scenario}
    options["resumes"] = any(step.get("resume") for step in steps(scenario))
    options["compacts"] = any(step.get("compact") for step in steps(scenario))
    plan = steps(scenario)
    last = {step["session"]: i for i, step in enumerate(plan)}
    sessions, records, closed, conversation = {}, [], {}, {}
    codex_memory = codex_memory_rows(sandbox)
    try:
        for index, step in enumerate(plan):
            name = step["session"]
            cwd = sandbox.workdir(step.get("cwd", scenario.get("cwd", "plain")), projects)
            resume = None
            if step.get("resume"):
                # A new process continuing the same conversation.
                if name in sessions:
                    closed[name] = sessions.pop(name).close()
                resume = conversation.get(name)
                if not resume:
                    raise RuntimeError(f"step {index} resumes {name}, which has no conversation yet")
            if name not in sessions:
                sessions[name] = open_session(client, sandbox, cwd, options, resume)
            started = time.time()
            prompt = "/compact" if step.get("compact") else step["prompt"]
            if step.get("compact"):
                events, error = sessions[name].compact()
            else:
                events, error = sessions[name].turn(prompt)
            record = {
                "step": index, "session": name, "client": client, "cwd": str(cwd),
                "resumed": bool(resume),
                "prompt": prompt, "error": error, "seconds": round(time.time() - started, 1),
                "tool_calls": tool_calls(client, events), "answer": final_answer(client, events),
                "commands": shell_commands(client, events), "file_writes": file_writes(client, events),
                "hook_context": hook_context(client, events),
                "approvals": list(getattr(sessions[name], "approvals", [])),
                "conversation_ids": sorted(session_ids(client, events)), "events": events,
            }
            if client == "codex":
                record["instruction_sources"] = sessions[name].instruction_sources
            record["origins"] = origins(record)
            record["files"] = watched(scenario, sandbox)
            conversation.setdefault(name, next(iter(record["conversation_ids"]), None))
            records.append(record)
            if last[name] == index:
                closed[name] = sessions.pop(name).close()
            guard.check(f"{scenario['name']}/{client}/step {index}",
                        [cid for r in records for cid in r["conversation_ids"]])
    finally:
        # A failed turn or a guard stop still ends every session it opened.
        for name in list(sessions):
            closed[name] = sessions.pop(name).close()
    memories = sandbox.memories()
    results = []
    for check in scenario["checks"]:
        if check.get("client", client) != client:
            continue
        informational = check.get("informational", scenario.get("informational", False))
        check = {k: v for k, v in check.items() if k not in ("client", "informational")}
        check = resolve(check, sandbox, memories)
        passed, detail = grade(check, sandbox, records, memories)
        results.append({"check": check, "passed": passed, "detail": detail,
                        "informational": informational})
    if client == "codex":
        # Codex's own memory competes with user-memory as Claude's auto
        # memory does; reported on every Codex trial, not yet graded.
        now = codex_memory_rows(sandbox)
        results.append({"check": {"codex_memory_unchanged": {}}, "passed": now == codex_memory,
                         "detail": f"rows before {codex_memory}, after {now}", "informational": True})
    return {"scenario": scenario["name"], "client": client, "sandbox": str(sandbox.dir),
            "passed": all(r["passed"] for r in results if not r["informational"]),
            "checks": results, "steps": records, "stderr": closed, "memories": memories,
            "setup": sandbox.setup_output}


def header_notes(scenario, client, sandbox, projects):
    """Runs the trial's session-start hook, as the client would, for each of
    the scenario's `rehearse` entries and checks the header it prints; no
    agent session runs. An entry may link projects to the made-up
    conversation first, and set the payload's `source` (resume)."""
    notes = []
    prefix = "claude:session/" if client == "claude" else "codex:thread/"
    for number, entry in enumerate(scenario.get("rehearse", [])):
        session = f"rehearsal-{number}"
        links = entry.get("links", [])
        if links:
            conn = sqlite3.connect(sandbox.db)
            with conn:
                conn.executemany(
                    "INSERT INTO conversation_projects(origin, project) VALUES (?, ?)",
                    [(prefix + session, key) for key in links])
            conn.close()
        cwd = sandbox.workdir(entry["cwd"], projects)
        payload = {"session_id": session, "hook_event_name": "SessionStart",
                   "source": entry.get("source", "startup"), "cwd": str(cwd)}
        out = subprocess.run([str(sandbox.binary), "session-start", "--client", client],
                             cwd=cwd, env=sandbox.env, input=json.dumps(payload),
                             capture_output=True, text=True, timeout=30)
        for pattern in entry["matches"]:
            if not re.search(pattern, out.stdout, re.MULTILINE):
                notes.append(f"session start in {entry['cwd']} lacks {pattern!r}: "
                             f"{out.stdout[:400]!r}")
    return notes


def rehearse(scenarios, clients, base, source, projects, guard):
    """Installs one trial per scenario and client and checks what setup
    wrote, without a session."""
    failures = 0
    for scenario in scenarios:
        for client in dict.fromkeys(c for c, _ in runs_of(scenario, clients)):
            sandbox = prepare(scenario, client, base, source, projects)
            hooks = scenario.get("hooks", True)
            notes = []
            path = sandbox.instruction_file()
            text = path.read_text() if path.exists() else ""
            if "Generated by user-memory" not in text:
                notes.append(f"no generated rules in {path}")
            for seed in scenario.get("seed", []):
                title = seed["args"][seed["args"].index("--title") + 1]
                if seed.get("rule") and title not in text:
                    notes.append(f"rule {title!r} missing from {path.name}")
            if client == "claude":
                settings_file = sandbox.claude_dir / "settings.json"
                settings = json.loads(settings_file.read_text()) if settings_file.exists() else {}
                events = sorted((settings.get("hooks") or {}).keys())
                want = ["SessionStart", "UserPromptSubmit"] if hooks else []
                if events != want:
                    notes.append(f"Claude hooks {events}, want {want}")
                state_file = sandbox.claude_dir / ".claude.json"
                state = json.loads(state_file.read_text()) if state_file.exists() else {}
                if "user-memory" not in (state.get("mcpServers") or {}):
                    notes.append("no user-memory MCP server in the trial's .claude.json")
            else:
                trust = {h["eventName"]: h["trustStatus"] for h in user_memory_hooks(sandbox)}
                want = {"sessionStart": "trusted", "userPromptSubmit": "trusted"} if hooks else {}
                if trust != want:
                    notes.append(f"Codex hook trust {trust}, want {want}")
                config_file = sandbox.codex_home / "config.toml"
                if "user-memory" not in (config_file.read_text() if config_file.exists() else ""):
                    notes.append("no user-memory MCP server in the trial's config.toml")
            notes += header_notes(scenario, client, sandbox, projects)
            guard.check(f"rehearsal {scenario['name']}/{client}")
            failures += bool(notes)
            print(f"{'ok  ' if not notes else 'FAIL'} {scenario['name']}/{client}"
                  + "".join(f"\n      {n}" for n in notes), flush=True)
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--client", choices=["claude", "codex", "both"], default="both")
    parser.add_argument("--scenario", action="append", default=[])
    parser.add_argument("--budget", type=int, default=100)
    parser.add_argument("--binary", default=str(ROOT / "target/debug/user-memory"))
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--rehearse", action="store_true",
                        help="install every trial and check it, running no session")
    parser.add_argument("--summarize", metavar="BATCH", help="rewrite a saved batch's summary")
    parser.add_argument("--codex-config", action="append", default=[], metavar="KEY=VALUE",
                        help="extra Codex -c setting, e.g. model_reasoning_summary=\"detailed\"")
    args = parser.parse_args()
    CODEX_EXTRA.extend(args.codex_config)
    for folder in [OUT] + ([Path(args.summarize)] if args.summarize else []):
        if folder.resolve().is_relative_to(ROOT):
            sys.exit(f"refusing: {folder} is inside the repository; reports and transcripts stay outside it")
    writable = [ROOT, Path("/tmp").resolve(), Path(tempfile.gettempdir()).resolve()]
    if any(TRIALS.resolve().is_relative_to(place) for place in writable):
        sys.exit(f"refusing: trials in {TRIALS} could be written by Codex's sandbox or land in the repository")
    if args.summarize:
        print(resummarize(Path(args.summarize)))
        return
    spec = json.loads((ROOT / "evals/scenarios.json").read_text())
    unknown = set(args.scenario) - {s["name"] for s in spec["scenarios"]}
    if unknown:
        sys.exit(f"unknown scenario(s): {', '.join(sorted(unknown))}")
    # Diagnostics answer one question and run only when named.
    scenarios = [s for s in spec["scenarios"]
                 if s["name"] in args.scenario or (not args.scenario and not s.get("diagnostic"))]
    clients = ["claude", "codex"] if args.client == "both" else [args.client]
    if args.list:
        for s in spec["scenarios"]:
            runs = runs_of(s, ["claude", "codex"])
            print(f"{s['name']:48} {session_opens(s)} session(s) x {len(runs)} run(s)  {s['summary']}")
        return
    planned = sum(session_opens(s) * len(runs_of(s, clients)) for s in scenarios)
    print(f"{planned} agent sessions planned; {counted_runs()} already counted; budget {args.budget}")
    for s in scenarios:
        runs = runs_of(s, clients)
        per = {c: sum(1 for rc, _ in runs if rc == c) * session_opens(s) for c in clients}
        print(f"  {s['name']:48} " + ", ".join(f"{c} {n}" for c, n in per.items() if n))
    problems = [p for p in (claude_token_problem() if "claude" in clients else None,
                            codex_login_problem() if "codex" in clients else None) if p]
    print("eval logins: " + ("; ".join(problems) if problems else "present"))
    binary = Path(args.binary).resolve()
    if not os.access(binary, os.X_OK):
        sys.exit(f"build the binary first: {binary}")
    if args.dry_run:
        print("guard watches:")
        for path, digest in fingerprint().items():
            print(f"  {digest[:16]}  {path}")
        print(f"  plus: no snapshot under {REAL_SESSIONS} named after a trial session")
        print(f"  plus: no new folder under {REAL_CLAUDE_PROJECTS} named after a trial path")
        return
    batch = OUT / time.strftime("%Y%m%d-%H%M%S")
    batch.mkdir(parents=True)
    trials = TRIALS / batch.name
    trials.mkdir(parents=True, mode=0o700)
    TRIALS.chmod(0o700)
    with open(batch / "guard.log", "w") as log:
        guard = Guard(log)
        guard.check("baseline re-read")
        if args.rehearse:
            failures = rehearse(scenarios, clients, trials, binary, spec.get("projects", {}), guard)
            print(f"\nrehearsal: {failures} trial setup(s) failed; report: {batch}")
            sys.exit(1 if failures else 0)
        if problems:
            sys.exit("refusing: " + "; ".join(problems))
        if counted_runs() + planned > args.budget:
            sys.exit("refusing: the batch would exceed the run budget")
        results = []
        for scenario in scenarios:
            for client, repeat in runs_of(scenario, clients):
                result = trial(scenario, client, trials, binary, spec.get("projects", {}), guard)
                result["repeat"] = repeat
                name = f"{scenario['name']}-{client}-{repeat}"
                (batch / f"{name}.json").write_text(json.dumps(result, indent=1))
                results.append(result)
                print(f"{'PASS' if result['passed'] else 'FAIL'} {name}", flush=True)
                for r in result["checks"]:
                    if not r["passed"]:
                        tag = " (informational)" if r["informational"] else ""
                        print(f"    {r['check']}{tag}: {r['detail']}", flush=True)
    print(summarize(batch, results))
    print(f"\nreport: {batch}")


def summarize(batch, results):
    """Writes and returns the pass-rate table of a batch."""
    summary = {}
    for r in results:
        key = (r["scenario"], r["client"])
        passed, total, real, sent = summary.get(key, (0, 0, 0, 0))
        sent_origins = [o for s in r["steps"] for o in s["origins"]]
        summary[key] = (passed + r["passed"], total + 1,
                        real + sum(o["real"] for o in sent_origins), sent + len(sent_origins))
    lines = [f"# Eval batch {batch.name}", "",
             "| scenario | client | passed | origins with the real conversation id |",
             "|---|---|---|---|"]
    lines += [f"| {s} | {c} | {p}/{t} | {f'{o}/{n}' if n else '-'} |"
              for (s, c), (p, t, o, n) in sorted(summary.items())]
    lines += ["", f"Agent sessions counted so far: {counted_runs()}"]
    text = "\n".join(lines) + "\n"
    (batch / "summary.md").write_text(text)
    return text


def resummarize(batch):
    """Recomputes a saved batch's summary from its trial files."""
    results = [json.loads(path.read_text()) for path in sorted(batch.glob("*-[0-9]*.json"))]
    return summarize(batch, results)


if __name__ == "__main__":
    main()
