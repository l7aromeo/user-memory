#!/usr/bin/env python3
"""Evaluate how real agents (Claude Code, Codex) use user memory.

Each trial installs the built `user-memory` binary into its own home with
`user-memory setup`, exactly as a user would (instruction files, hooks,
skill, MCP registration), seeds its own store, runs real agent sessions in
that home, and grades the final store, the agents' tool calls and commands,
their answers and the files they left.

Usage:
    evals/run.py [--client claude|codex|both] [--scenario NAME ...]
                 [--budget N] [--top-models] [--list] [--dry-run] [--rehearse]

Every agent session is a billed run. `--dry-run` prints the plan, the login
files and the guard without running anything; `--rehearse` also installs
every trial, checks what setup wrote and grades each scenario's checks on
its planted transcripts, still without a single session.
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

Reports, raw transcripts and the run counter go to $USER_MEMORY_EVAL_OUT
(default: ~/Library/Caches/user-memory-eval/reports on macOS, else
$XDG_STATE_HOME/user-memory-eval/reports or ~/.local/state/...), a folder
a restart keeps, never into the repository.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import queue
import random
import re
import secrets
import shlex
import shutil
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# The stand-in model rehearsals run the real clients against.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import fake_model  # noqa: E402
HOME = Path.home()


def default_out(platform, environ, home):
    """Where reports and the run counter go unless USER_MEMORY_EVAL_OUT says:
    a per-user folder a restart keeps, never /tmp, whose emptying would reset
    the budget. On macOS beside the trial homes; elsewhere the XDG state
    folder."""
    if platform == "darwin":
        return home / "Library/Caches/user-memory-eval/reports"
    state = environ.get("XDG_STATE_HOME") or str(home / ".local/state")
    return Path(state) / "user-memory-eval/reports"


OUT = Path(os.environ.get("USER_MEMORY_EVAL_OUT") or default_out(sys.platform, os.environ, HOME))

# Each batch runs the cheaper model of each provider's latest family: a pass
# then says more about memory, the batch costs less, and the names follow new
# families. `--top-models` picks the top tier instead, as for re-running a
# failure before calling it a bug. Trials never read the user's own settings,
# so the model is always passed explicitly.
# Claude Code resolves its `sonnet` and `opus` aliases to the newest model of
# that tier, so these never age.
CLAUDE_DEFAULT_MODEL = "sonnet"
CLAUDE_TOP_MODEL = "opus"
# Codex has no such alias: each tier is named once here, and `--dry-run` and
# `--rehearse` stop when Codex reports either one superseded.
CODEX_DEFAULT_MODEL = "gpt-6-luna"
# The top tier reruns a failure on what the user's real sessions run, as
# Opus does for Claude Code: Codex's own default model.
CODEX_TOP_MODEL = "gpt-6.1-sol"
CODEX_EFFORT = os.environ.get("USER_MEMORY_EVAL_CODEX_EFFORT", "high")
# Per 1M tokens: (input, cached input, output) in US dollars, for the batch
# report's cost estimate of Codex trials (Claude Code reports its own). A
# model missing here is reported with tokens only.
CODEX_PRICES = {"gpt-6-luna": (0.10, 0.01, 0.50)}


def models(top, environ):
    """(Claude model, Codex model) a batch runs: the cheaper tier, the top one
    with `top`, or what USER_MEMORY_EVAL_CLAUDE_MODEL and
    USER_MEMORY_EVAL_CODEX_MODEL name."""
    claude = CLAUDE_TOP_MODEL if top else CLAUDE_DEFAULT_MODEL
    codex = CODEX_TOP_MODEL if top else CODEX_DEFAULT_MODEL
    return (environ.get("USER_MEMORY_EVAL_CLAUDE_MODEL") or claude,
            environ.get("USER_MEMORY_EVAL_CODEX_MODEL") or codex)


# Set by main() from the command line and the environment.
CLAUDE_MODEL, CODEX_MODEL = models(False, {})
# One agent turn; a turn that takes longer is cut off and graded as is.
TURN_TIMEOUT = 600
# What makes a debug build of user-memory report another release: a
# scenario's `versions` sets it for the client and each server.
TEST_VERSION = "USER_MEMORY_TEST_VERSION"
# Starting a Codex app-server and answering one request.
CODEX_REQUEST_TIMEOUT = 60

# Trials (homes, stores, binaries, projects) live here, one 0700 folder per
# batch: outside /tmp and TMPDIR, which Codex's workspace sandbox may write,
# so a sandboxed command cannot plant a program a trial later runs outside
# it. OUT holds only reports.
TRIALS = Path(os.environ.get("USER_MEMORY_EVAL_TRIALS",
                             HOME / "Library/Caches/user-memory-eval/trials"))
# Trial batches kept in TRIALS, the new one included: each holds whole homes,
# binaries and projects (about 0.7 GB), so old ones are removed when a batch
# starts. Enough to inspect the last few batches; USER_MEMORY_EVAL_KEEP_TRIALS
# keeps more.
KEPT_BATCHES = 3
# A batch folder's name, the time it started, as main() names it; nothing
# else in TRIALS is ever removed.
BATCH_NAME = re.compile(r"\d{8}-\d{6}")
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


# The trials' optional semantic search: USER_MEMORY_EVAL_EMBED_<NAME> becomes
# USER_MEMORY_EMBED_<NAME> in each trial, where setup saves it. Unset, the
# trials run keyword-only.
EMBED_NAMES = ("URL", "MODEL", "DIMENSIONS", "QUERY_INSTRUCTION")


def embed_settings(environ):
    """The embedding settings a trial's setup receives, from `environ`."""
    settings = {f"USER_MEMORY_EMBED_{name}": environ[f"USER_MEMORY_EVAL_EMBED_{name}"]
                for name in EMBED_NAMES if environ.get(f"USER_MEMORY_EVAL_EMBED_{name}")}
    if not settings:
        return {}
    url = settings.get("USER_MEMORY_EMBED_URL")
    if not url:
        sys.exit("USER_MEMORY_EVAL_EMBED_URL is needed for the other USER_MEMORY_EVAL_EMBED_ settings")
    if url.startswith("http://"):
        # user-memory allows plain http only when told, and only to this machine.
        settings["USER_MEMORY_EMBED_ALLOW_HTTP"] = "1"
    return settings


def kept_batches(environ):
    """How many trial batches to keep: USER_MEMORY_EVAL_KEEP_TRIALS, or KEPT_BATCHES."""
    value = environ.get("USER_MEMORY_EVAL_KEEP_TRIALS")
    if value is None:
        return KEPT_BATCHES
    if not value.isdigit() or int(value) < 1:
        sys.exit(f"USER_MEMORY_EVAL_KEEP_TRIALS must be a whole number of at least 1, not {value!r}")
    return int(value)


def prune_trials(root, keep):
    """Removes all but the newest `keep` batch folders in `root`; returns their names.

    Only real folders named like a batch go: a symlink, a file or any other
    name is left alone. A trial may leave read-only or unreadable folders
    (git objects, a tool's cache), so every folder in a batch is opened up
    first; symlinks are never followed."""
    batches = sorted(p.name for p in root.iterdir()
                     if BATCH_NAME.fullmatch(p.name) and p.is_dir() and not p.is_symlink())
    old = batches[:-keep]
    for name in old:
        top = root / name
        top.chmod(0o700)
        for folder, children, _files in os.walk(top):
            for child in children:
                path = os.path.join(folder, child)
                if not os.path.islink(path):
                    os.chmod(path, 0o700)
        shutil.rmtree(top)
    return old


def start_trials(name, keep):
    """Creates the batch folder `name` in TRIALS, 0700, and prunes older batches."""
    trials = TRIALS / name
    trials.mkdir(parents=True, mode=0o700)
    TRIALS.chmod(0o700)
    prune_trials(TRIALS, keep)
    return trials


def trial_projects():
    """Folders in the real ~/.claude/projects named after a trial path."""
    slug = re.sub(r"[^A-Za-z0-9]", "-", str(TRIALS.resolve())).strip("-")
    if not REAL_CLAUDE_PROJECTS.is_dir():
        return set()
    return {p.name for p in REAL_CLAUDE_PROJECTS.iterdir() if slug in p.name}


# Where Claude Code keeps each session's task output files, as
# `<base>/<slug of the session's cwd>/<session id>/tasks/`: the user's
# fixed temporary folder, whatever TMPDIR says (Claude Code 2.1.325,
# measured), so a trial's files land outside its sandbox. Other Claude Code
# sessions, the user's own among them, keep theirs there too.
CLAUDE_TMP_BASE = Path(f"/tmp/claude-{os.getuid()}") if hasattr(os, "getuid") else None


def path_slug(path):
    """A path as Claude Code names a folder after it: every character but
    a letter or digit as `-`."""
    return re.sub(r"[^A-Za-z0-9]", "-", str(path))


def clear_claude_tmp(trial_dir, base=None):
    """Removes the folders Claude Code made under `base` (CLAUDE_TMP_BASE)
    for sessions whose cwd is in `trial_dir`, and no other; returns those
    left there named after another trial of its batch, the folder holding
    `trial_dir`. A batch runs its trials one at a time, so each of those has
    finished; another batch, which may be running, is not looked at."""
    base = CLAUDE_TMP_BASE if base is None else base
    if base is None or not base.is_dir():
        return []
    trial_dir = Path(trial_dir).resolve()
    own = path_slug(trial_dir)
    for entry in base.iterdir():
        if (entry.name == own or entry.name.startswith(own + "-")) and entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry)
    batch = path_slug(trial_dir.parent) + "-"
    return sorted(str(entry) for entry in base.iterdir() if entry.name.startswith(batch))


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

def counted_runs(out=None):
    """Sessions recorded in the output folder's run counter. A folder without
    one counts 0 only when no batch there ran a session: a lost counter would
    otherwise let `--budget` allow a full budget again."""
    out = OUT if out is None else out
    try:
        return int((out / "run-count").read_text())
    except FileNotFoundError:
        ran = [b for b in out.glob("*") if b.is_dir() and BATCH_NAME.fullmatch(b.name)
               and any(b.glob("*-[0-9]*.json"))]
        if ran:
            sys.exit(f"refusing: {out} holds {len(ran)} batches with session results but no "
                     f"run-count; write the sessions already run there to {out / 'run-count'}")
        return 0


def budget_line(planned, budget, out=None):
    """The plan's first line: sessions planned, the count read and the
    counter file it was read from, and the budget."""
    out = OUT if out is None else out
    return (f"{planned} agent sessions planned; {counted_runs(out)} already counted "
            f"({out / 'run-count'}); budget {budget}")


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
        # The default state folder under HOME, which XDG_STATE_HOME also
        # names: Claude Code gives hooks no XDG variable (2.1.325, measured),
        # so they fall back to it, and every process writes in one place.
        self.state = self.home / ".local" / "state"
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
            **embed_settings(os.environ),
        }
        self.ids = {}
        # The server this trial's client connects to, if any, and the
        # client's copy of its token.
        self.remote = None
        self.token_file = self.dir / "client-token"

    def cli(self, *args):
        binary = self.binary if self.binary.exists() else self.source
        out = subprocess.run(
            [str(binary), *args], env=self.env, capture_output=True, text=True, timeout=60
        )
        if out.returncode != 0:
            raise RuntimeError(f"user-memory {' '.join(args)} failed: {out.stderr.strip()}")
        return out.stdout

    def client_env(self):
        """The environment the client and what it starts run in: without the
        store's path when the trial connects to a server, since a store
        path set there means a local store."""
        if self.remote is None:
            return self.env
        return {k: v for k, v in self.env.items() if k != "USER_MEMORY_DB"}

    def setup(self, hooks):
        """Installs user-memory for this trial's client, as a user would:
        with its own store, or connected to the trial's server."""
        args = ["setup", "--client", self.client, "--install-dir", str(self.install)]
        if self.remote is None:
            args += ["--db", str(self.db)]
        else:
            args += ["--url", self.remote.url, "--token-file", str(self.token_file)]
        if not hooks:
            args.append("--no-hooks")
        out = subprocess.run([str(self.source), *args], env=self.client_env(), capture_output=True,
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
        """Messages of the commits the agent made: those after the project's
        first one and its `history` (see `write_files`)."""
        if not (path / ".git").exists():
            return []
        start = getattr(self, "starts", {}).get(Path(path).resolve())
        out = subprocess.run([GIT, "log", "--format=%B%x00", f"{start}..main" if start else "main"], cwd=path, env={
            **self.env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"},
            capture_output=True, text=True)
        messages = [m.strip() for m in out.stdout.split("\0") if m.strip()]
        return messages if start else messages[:-1]

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


def codex_models():
    """Codex's model list (`model/list`, hidden ones included), asked of the
    installed Codex in an empty home: it answers from the list it ships."""
    with tempfile.TemporaryDirectory() as home:
        (Path(home) / ".codex").mkdir()
        env = {"HOME": home, "CODEX_HOME": f"{home}/.codex", "PATH": "/usr/bin:/bin",
               "LANG": os.environ.get("LANG", "en_US.UTF-8")}
        reply = app_server_request(env, [
            {"id": 0, "method": "initialize", "params": {"clientInfo": {"name": "eval", "version": "1"}}},
            {"method": "initialized"},
            {"id": 1, "method": "model/list", "params": {"includeHidden": True}},
        ], 1)
    if "error" in reply:
        raise RuntimeError(f"codex model/list failed: {reply['error']}")
    return reply["result"]["data"]


def stale_models(listed, names):
    """Why any of `names` should not run, from Codex's model list `listed`:
    one Codex no longer lists, or one whose `upgrade` names its successor."""
    by_name = {m.get("model") or m.get("id"): m for m in listed}
    problems = []
    for name in dict.fromkeys(names):
        model = by_name.get(name)
        if model is None:
            problems.append(f"Codex lists no model {name}")
        elif model.get("upgrade"):
            problems.append(f"Codex reports {name} superseded by {model['upgrade']}: "
                            "name the newer model in evals/run.py")
    return problems


def user_memory_hooks(sandbox):
    """The trial's user-memory hooks as Codex lists them."""
    reply = app_server_request(sandbox.client_env(), [
        {"id": 0, "method": "initialize", "params": {"clientInfo": {"name": "eval", "version": "1"}}},
        {"method": "initialized"},
        {"id": 1, "method": "hooks/list", "params": {"cwds": [str(sandbox.dir)]}},
    ], 1)
    return [hook for entry in reply["result"]["data"] for hook in entry["hooks"]
            if "user-memory" in hook.get("command", "")]


# The events setup gives user-memory's hooks in each client: Claude Code's
# settings keys, and Codex's events as `hooks/list` names them, with how
# many handlers Codex lists (two on session start: the index and, on
# compact, the working set).
CLAUDE_HOOK_EVENTS = ["PostToolUse", "PostToolUseFailure", "PreToolUse", "SessionStart",
                      "SubagentStart", "UserPromptSubmit"]
CODEX_HOOK_EVENTS = ["postToolUse", "preToolUse", "sessionStart", "subagentStart",
                     "userPromptSubmit"]
CODEX_HOOK_HANDLERS = 6


def approve_codex_hooks(sandbox):
    """Approves the trial's user-memory hooks the way a user does in /hooks:
    Codex records each hook's current hash as trusted in config.toml."""
    hooks = user_memory_hooks(sandbox)
    if len(hooks) != CODEX_HOOK_HANDLERS:
        raise RuntimeError(f"Codex lists {len(hooks)} user-memory hooks, not {CODEX_HOOK_HANDLERS}")
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


def claude_permissions(sandbox, cwd, options):
    """(tools, allowed, denied) for a trial's Claude Code session in `cwd`:
    its built-in tools, and the allow and deny rules for its calls."""
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
        # And the commands the scenario's prompt and notes name, such as
        # `git tag *` or `python3 tools/*`, which the shape rules leave out;
        # the deny rules still apply to them.
        tools = CLAUDE_SHELL_TOOLS
        allowed = allowed + allow + [f"Bash({pattern})" for pattern in options.get("allow", [])]
    elif options.get("claude_file_tools"):
        tools = CLAUDE_FILE_TOOLS
    if options.get("subagents"):
        # The subagent tool; what a subagent may do is what the session's
        # other rules allow.
        tools = ",".join([t for t in tools.split(",") if t] + ["Agent"])
        allowed = allowed + ["Agent"]
    if options.get("asks"):
        # The question tool, answered by the harness as the user would:
        # with a host for prompts, every call no rule allows comes to
        # the harness, which declines all but the questions.
        tools = ",".join([t for t in tools.split(",") if t] + ["AskUserQuestion"])
    return tools, allowed, denied


class ClaudeSession:
    """A Claude Code session that stays open between turns: each turn is a
    user message on stdin, answered by a `result` event."""

    # How long one turn may take: TURN_TIMEOUT, or the scenario's own.
    turn_timeout = TURN_TIMEOUT

    def __init__(self, sandbox, cwd, options, resume=None):
        self.turn_timeout = options.get("turn_timeout", TURN_TIMEOUT)
        tools, allowed, denied = claude_permissions(sandbox, cwd, options)
        asks = options.get("asks")
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
            *(["--permission-prompt-tool", "stdio"] if asks else []),
        ]
        if options.get("claude_file_tools"):
            self.command += ["--settings", json.dumps({"autoMemoryEnabled": True})]
        # No self-update of the user's installed Claude Code mid-batch.
        # The token signs the session in; the scrub keeps it out of every
        # command, hook and MCP server Claude starts, so no command can put it
        # in a commit or a result.
        env = {**sandbox.client_env(), "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB": "1", "DISABLE_AUTOUPDATER": "1"}
        if options.get("fake_model"):
            # A rehearsal: the fake model on loopback, a placeholder key, no
            # login, and no traffic beyond the model's.
            env.update({"ANTHROPIC_BASE_URL": options["fake_model"], "ANTHROPIC_API_KEY": "fake",
                        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"})
        else:
            env["CLAUDE_CODE_OAUTH_TOKEN"] = claude_token()
        self.questions, self.declined = [], []
        self.child = subprocess.Popen(self.command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.lines = reader(self.child.stdout)
        self.errors = reader(self.child.stderr)

    def say(self, text):
        self.child.stdin.write(json.dumps({"type": "user", "message": {"role": "user", "content": text}}) + "\n")
        self.child.stdin.flush()

    def turn(self, prompt, while_working=None, answers=None):
        """One turn: `prompt`, then `while_working` as a second user message
        once the agent's first tool call shows, as a user types while the
        agent works; Claude Code adds it to the running turn. Its question
        tool is answered from `answers`."""
        events, error, background = [], None, []
        self.questions, self.declined, self.steered = [], [], False
        try:
            self.say(prompt)
        except BrokenPipeError:
            return events, "the session had ended"
        deadline = time.time() + self.turn_timeout
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                error = f"timed out after {self.turn_timeout}s"
                break
            if line is None:
                error = "the session ended"
                break
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            events.append(event)
            if event.get("type") == "control_request":
                self.control(event, answers or [])
            elif while_working and not self.steered and tool_uses([event]):
                self.say(while_working)
                self.steered = True
            if event.get("type") == "system" and event.get("subtype") == "background_tasks_changed":
                background = event.get("tasks") or []
            # A background subagent's result comes back as a turn of its
            # own, a `<task-notification>` prompt, so the step lasts until
            # a result with no background task left.
            if event.get("type") == "result" and not background:
                break
        return events, error

    def control(self, event, answers):
        """Answers Claude Code's request to its host: a question from its
        question tool as `answers` say, or declined when none fits; any other
        tool call, which no allow rule covers, declined, as Claude Code
        declines it with no one to ask."""
        request = event.get("request") or {}
        if request.get("subtype") != "can_use_tool":
            reply = {"subtype": "error", "request_id": event["request_id"],
                     "error": "the eval client does not support this request"}
        else:
            if request.get("tool_name") == "AskUserQuestion":
                asked = request.get("input", {}).get("questions", [])
                picked = pick_answers([(q.get("question", ""), [o.get("label", "") for o in q.get("options", [])])
                                       for q in asked], answers)
                self.questions.append({"questions": [q.get("question", "") for q in asked], "answers": picked})
                decision = ({"behavior": "allow", "updatedInput": {**request["input"], "answers": picked}}
                            if picked is not None else
                            {"behavior": "deny", "message": "The user declined to answer."})
            else:
                self.declined.append(request.get("tool_name"))
                decision = {"behavior": "deny", "message": "Not allowed in this session."}
            reply = {"subtype": "success", "request_id": event["request_id"], "response": decision}
        self.child.stdin.write(json.dumps({"type": "control_response", "response": reply}) + "\n")
        self.child.stdin.flush()

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


def tool_uses(events):
    """Claude tool calls in `events`."""
    return [part for event in events if event.get("type") == "assistant"
            for part in (event.get("message") or {}).get("content", [])
            if part.get("type") == "tool_use"]


def pick_answers(questions, answers):
    """The user's answers to `questions` ((text, option labels) pairs), from
    a step's `answers` ({"question": REGEX, "pick": REGEX}, first match
    wins): {question: label}, or None, declining them all, when one has no
    entry or no option that entry picks."""
    picked = {}
    for text, labels in questions:
        entry = next((a for a in answers if re.search(a["question"], text, re.IGNORECASE)), None)
        label = entry and next((l for l in labels if re.search(entry["pick"], l, re.IGNORECASE)), None)
        if not label:
            return None
        picked[text] = label
    return picked


def toml_string(value):
    return json.dumps(str(value))


CODEX_EXTRA = []


class CodexSession:
    """A Codex session through `codex app-server`: one thread, one turn per
    prompt. Its configuration is the trial's CODEX_HOME, as setup wrote it."""

    # How long one turn may take: TURN_TIMEOUT, or the scenario's own.
    turn_timeout = TURN_TIMEOUT

    def __init__(self, sandbox, cwd, options, resume=None):
        self.turn_timeout = options.get("turn_timeout", TURN_TIMEOUT)
        self.sandbox = sandbox
        self.resume = resume
        self.child = None
        self.questions, self.answers, self.steered = [], [], False
        link = sandbox.codex_home / "auth.json"
        link.unlink(missing_ok=True)
        if not options.get("fake_model"):
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
        env = self.sandbox.client_env()
        if options.get("fake_model"):
            # A rehearsal: a provider at the fake model on loopback, with a
            # placeholder key and no login.
            provider = (f'{{name="fake",base_url="{options["fake_model"]}/v1",'
                        f'wire_api="responses",env_key="USER_MEMORY_EVAL_FAKE_KEY"}}')
            self.command += ["-c", f"model_providers.fake={provider}", "-c", 'model_provider="fake"']
            env = {**env, "USER_MEMORY_EVAL_FAKE_KEY": "fake"}
        self.child = subprocess.Popen(self.command, cwd=cwd, env=env,
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
            # Every thread is kept in the trial's CODEX_HOME, as a user's is:
            # a scenario may resume it, and the post-tool hook reads a
            # call's result from its transcript, which an ephemeral thread
            # never writes.
            started = self.request("thread/start", {**thread, "ephemeral": False})
        result = started.get("result") or {}
        self.thread = (result.get("thread") or {}).get("id")
        self.model = result.get("model") or CODEX_MODEL
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
        if method == "item/tool/requestUserInput":
            # The question tool, answered as the step's `answers` say.
            asked = params.get("questions") or []
            picked = pick_answers([(q.get("question", ""), [o.get("label", "") for o in q.get("options") or []])
                                   for q in asked], self.answers)
            self.questions.append({"questions": [q.get("question", "") for q in asked], "answers": picked})
            by_id = {q.get("id"): picked[q.get("question", "")] for q in asked} if picked else {}
            self.send({"id": message["id"], "result": {
                "answers": {qid: {"answers": [label]} for qid, label in by_id.items()}}})
            return
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
        deadline = time.time() + self.turn_timeout
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                return events, f"timed out after {self.turn_timeout}s"
            if line is None:
                return events, "codex app-server ended"
            message = json.loads(line)
            self.answer(message)
            events.append(message)
            if message.get("method") in ("thread/compacted", "turn/completed"):
                return events, None

    def turn(self, prompt, while_working=None, answers=None):
        """One turn: `prompt`, then `while_working` added to the running turn
        (turn/steer) once the agent's first tool call starts, as a user types
        while the agent works. Its question tool is answered from
        `answers`."""
        events, error = [], None
        self.approvals, self.questions, self.answers, self.steered = [], [], answers or [], False
        reply = self.request("turn/start", {"threadId": self.thread,
                                            "input": [{"type": "text", "text": prompt}]}, events)
        if "error" in reply:
            return events, f"turn/start failed: {reply['error']}"
        turn_id = ((reply.get("result") or {}).get("turn") or {}).get("id")
        deadline = time.time() + self.turn_timeout
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                error = f"timed out after {self.turn_timeout}s"
                break
            if line is None:
                error = "codex app-server ended"
                break
            message = json.loads(line)
            self.answer(message)
            events.append(message)
            if while_working and not self.steered and codex_tool_started(message):
                self.next_id += 1
                self.send({"id": self.next_id, "method": "turn/steer", "params": {
                    "threadId": self.thread, "expectedTurnId": turn_id,
                    "input": [{"type": "text", "text": while_working}]}})
                self.steered = True
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
    "commit": {"-s", "--signoff"},
    "tag": set(),
    "show-ref": set(),
    "rev-parse": set(),
}
# Steps that change the repository; an approved command holds at least one.
WRITES = {"add", "commit", "tag"}
# A tag name an approved command may make or list: a release version.
TAG_NAME = r"v?\d+(\.\d+){1,3}"
SHELLS = {"sh", "bash", "zsh"}
OPERATORS = set("();<>|&")


def escalation(command, cwd, trial):
    """Whether to let Codex run `command` outside its sandbox: git steps
    joined by && or ;, each with only the options STEP_OPTIONS allows, file
    paths inside the trial, at least one add, commit or tag, and a commit
    message given only with -m or --message; run in a directory of the
    trial."""
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
    # Whole-command shapes, checked before the loop so that no arguments
    # is a shape too.
    if sub == "tag":
        # One release tag made, or listed; no option that deletes, moves or
        # signs one.
        return (re.fullmatch(TAG_NAME, " ".join(args)) is not None
                or (len(args) == 2 and args[0] in ("-l", "--list") and re.fullmatch(TAG_NAME, args[1]) is not None)
                or args == ["--points-at", "HEAD"])
    if sub == "show-ref":
        # Where one release tag points, read only.
        return len(args) == 2 and args[0] == "--tags" and re.fullmatch(TAG_NAME, args[1]) is not None
    if sub == "rev-parse":
        # The commit HEAD names, read only.
        return args == ["HEAD"]
    i = 0
    while i < len(args):
        arg = args[i]
        if sub == "commit":
            if arg in ("-m", "--message") and i + 1 < len(args):
                i += 2
                continue
            if arg.startswith("--message=") or arg in allowed:
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


# Items of a Codex turn that are not tool calls.
NOT_TOOLS = {"userMessage", "agentMessage", "reasoning", "plan"}


def codex_tool_started(message):
    """Whether `message` reports a Codex tool call starting."""
    item = (message.get("params") or {}).get("item") or {} if message.get("method") == "item/started" else {}
    return bool(item) and item.get("type") not in NOT_TOOLS


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


# Claude's tools that read files, and the input naming what they read.
READ_TOOLS = {"Read": "file_path", "Grep": "path", "Glob": "path", "NotebookRead": "notebook_path"}


def file_reads(client, events):
    """Paths a Claude turn tried to read with its built-in file tools; Codex
    reads through shell commands, which `commands` holds."""
    return [str(part.get("input", {}).get(READ_TOOLS[part.get("name")]) or "")
            for part in tool_uses(events) if part.get("name") in READ_TOOLS] if client == "claude" else []


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
    entries of each hook run Codex reports, each with the hook event that
    produced it."""
    if client != "codex":
        return []
    return [{"event": (e["params"].get("run") or {}).get("eventName", ""), "text": entry.get("text", "")}
            for e in events
            if e.get("method") == "hook/completed" and isinstance(e.get("params"), dict)
            for entry in (e["params"].get("run") or {}).get("entries") or []
            if entry.get("kind") == "context"]


# Each client's own tool that starts a subagent: Claude Code's tool use, and
# the thread item Codex's app-server reports for it: up to 0.163.0 a collab
# tool call with the prompt, from 0.171.0 the sub-agent's activity, whose
# `kind` is "started" when it is spawned and "interacted" for each message
# sent to it, and which holds no prompt.
CLAUDE_SUBAGENT_TOOLS = {"Agent", "Task"}
CODEX_SUBAGENT_ITEM = "collabAgentToolCall"
CODEX_SUBAGENT_SPAWN = "spawnAgent"
CODEX_SUBAGENT_ACTIVITY = "subAgentActivity"
CODEX_SUBAGENT_STARTED = "started"


def subagents(client, events):
    """The subagents a turn started, each as {"tool", "prompt"}: a client's
    own tool, which the memory tool calls never include."""
    if client == "claude":
        return [{"tool": part["name"], "prompt": str((part.get("input") or {}).get("prompt", ""))}
                for part in tool_uses(events) if part.get("name") in CLAUDE_SUBAGENT_TOOLS]
    started = []
    for item in items(events):
        if item.get("type") == CODEX_SUBAGENT_ITEM and item.get("tool") == CODEX_SUBAGENT_SPAWN:
            started.append({"tool": item["tool"], "prompt": str(item.get("prompt") or "")})
        elif item.get("type") == CODEX_SUBAGENT_ACTIVITY and item.get("kind") == CODEX_SUBAGENT_STARTED:
            started.append({"tool": CODEX_SUBAGENT_ACTIVITY, "prompt": ""})
    return started


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


# How much of a failed turn's message a step keeps: enough for the reason a
# provider gives, short of a whole error page.
FAILURE_TEXT = 300


def turn_failure(client, events):
    """Why the client ended a turn as failed, or None. Claude Code marks
    its `result` event `is_error` and puts the error in its text; Codex
    sends an `error` notification it won't retry and ends the turn
    `failed`. Without this a failed turn reads as an answer (Claude's error
    text) or an empty one (Codex), and is graded as the agent's choice."""
    if client == "claude":
        result = next((e for e in reversed(events) if e.get("type") == "result"), None)
        if result and (result.get("is_error") or str(result.get("subtype", "")).startswith("error")):
            return (f"Claude Code ended the turn as an error ({result.get('subtype')}): "
                    f"{str(result.get('result') or '')[:FAILURE_TEXT]}")
        return None
    for message in reversed(events):
        params = message.get("params") or {}
        turn = params.get("turn") or {}
        if message.get("method") == "turn/completed" and turn.get("status") not in (None, "completed"):
            reason = (turn.get("error") or {}).get("message") or ""
            return f"Codex ended the turn {turn['status']}: {reason[:FAILURE_TEXT]}"
        if message.get("method") == "error" and not params.get("willRetry"):
            reason = (params.get("error") or {}).get("message") or ""
            return f"Codex reported an error: {reason[:FAILURE_TEXT]}"
    return None


def final_answer(client, events):
    if client == "claude":
        for event in reversed(events):
            if event.get("type") == "result":
                return event.get("result") or ""
        return ""
    texts = [item.get("text", "") for item in items(events) if item.get("type") == "agentMessage"]
    return texts[-1] if texts else ""


# What a usage record counts, per model.
USAGE_FIELDS = ("input", "cached_input", "output", "reasoning", "cost_usd")


def claude_totals(events):
    """A Claude conversation's running usage per model after a turn: its
    last `result` event's modelUsage, which counts every turn so far, the
    turns of the process a resume replaced included. Input counts cached and
    cache-written tokens too; the cost is Claude Code's own estimate."""
    result = next((e for e in reversed(events) if e.get("type") == "result"), None)
    totals = {}
    for model, used in ((result or {}).get("modelUsage") or {}).items():
        cached = used.get("cacheReadInputTokens", 0)
        totals[model] = {
            "input": used.get("inputTokens", 0) + cached + used.get("cacheCreationInputTokens", 0),
            "cached_input": cached, "output": used.get("outputTokens", 0),
            "reasoning": used.get("thinkingTokens", 0), "cost_usd": used.get("costUSD"),
        }
    return totals


def codex_totals(events, model):
    """A Codex thread's running usage after a turn: the `total` of its last
    thread/tokenUsage/updated notification, which a resumed thread carries
    on. Input counts cached tokens and output counts reasoning, as OpenAI
    bills them; the cost comes from CODEX_PRICES, or is None."""
    update = next((e for e in reversed(events) if e.get("method") == "thread/tokenUsage/updated"), None)
    if update is None:
        return {}
    total = update["params"]["tokenUsage"]["total"]
    used = {"input": total.get("inputTokens", 0), "cached_input": total.get("cachedInputTokens", 0),
            "output": total.get("outputTokens", 0), "reasoning": total.get("reasoningOutputTokens", 0)}
    price = CODEX_PRICES.get(model)
    used["cost_usd"] = None if price is None else (
        (used["input"] - used["cached_input"]) * price[0] + used["cached_input"] * price[1]
        + used["output"] * price[2]) / 1_000_000
    return {model: used}


def usage_since(now, before):
    """What a step used: running totals `now` less those `before` it, per
    model."""
    step = {}
    for model, used in now.items():
        earlier = before.get(model, {})
        step[model] = {field: None if used.get(field) is None else
                       used[field] - (earlier.get(field) or 0) for field in USAGE_FIELDS}
    return step


def add_usage(into, usage):
    """Adds `usage` (per model) into `into`; a cost stays None once one is
    unknown."""
    for model, used in usage.items():
        total = into.setdefault(model, {field: 0 for field in USAGE_FIELDS})
        for field in USAGE_FIELDS:
            known = total[field] is not None and used.get(field) is not None
            total[field] = total[field] + used[field] if known else None
    return into


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


def runs_of(scenario, clients, repeat=None):
    """(client, repeat) pairs a batch runs for `scenario`: its own `repeat`,
    or `repeat` when the batch sets one for every scenario."""
    names = [c for c in clients if c in scenario.get("clients", ["claude", "codex"])]
    return [(c, r) for r in range(repeat or scenario.get("repeat", 1)) for c in names]


# ---------------------------------------------------------------- grading

def matches(memory, where):
    for field, pattern in where.items():
        value = memory.get(field)
        if value is None:
            return False
        if not re.search(pattern, str(value), re.IGNORECASE):
            return False
    return True


# The agent context of a session's own agent in a `.recall` file; any other
# is a subagent's id.
RECALL_MAIN = "main"


def recall_state(sandbox):
    """What the trial's hooks pushed, from each session's `.recall` file:
    the `shown` rows (file, agent, memory, passage, step), leaving out the
    rules session start printed in full (passage -1), which no hook pushed;
    and each agent's characters pushed in its current epoch, the most any
    session holds."""
    rows, chars = set(), {}
    for path in sorted((sandbox.state / "user-memory" / "sessions").glob("*.recall")):
        conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        try:
            rows |= {(path.name, *row) for row in conn.execute(
                "SELECT agent, memory, passage, at FROM shown WHERE passage >= 0")}
            for agent, used in conn.execute("SELECT agent, chars FROM contexts"):
                chars[agent] = max(chars.get(agent, 0), used)
        except sqlite3.Error:
            # A file a hook is replacing, or of another layout: what it held
            # is not graded, and the check that needed it fails.
            pass
        finally:
            conn.close()
    return rows, chars


def pushes_since(rows, seen):
    """The pushes in `rows` not in `seen`: each {"agent", "agent_id",
    "memory", "passage"}, the agent "main" or "subagent". A passage pushed
    again, after a compaction or a stretch, is a new row."""
    return [{"agent": "main" if agent == RECALL_MAIN else "subagent", "agent_id": agent,
             "memory": memory, "passage": passage}
            for _, agent, memory, passage, _ in sorted(rows - seen)]


def pushed_matches(push, spec):
    """Whether `push` is one `spec` names: its memory (a short id or more),
    agent, and passage, or any passage but `passage_not`."""
    memory = spec.get("memory")
    if memory and not (push["memory"].startswith(memory) or memory.startswith(push["memory"])):
        return False
    if spec.get("agent") not in (None, push["agent"]):
        return False
    if "passage" in spec and push.get("passage") != spec["passage"]:
        return False
    return not ("passage_not" in spec and push.get("passage") == spec["passage_not"])


# What opens every push a hook prints (src/memory/push.rs's HEADER and
# FAILED_HEADER).
PUSH_HEADER = re.compile(r"^From user-memory, matching this (?:failed )?step ", re.M)


def unseen_pushes(record):
    """The pushes a Codex step's `.recall` file records that no hook output
    of the step shows: a push line under a push header, naming the memory's
    short id. Each is a harness error, so a `pushed` check never passes on
    the state file's bookkeeping alone. Claude Code reports no hook output,
    so nothing is compared there."""
    if record.get("client") != "codex":
        return []
    texts = [e["text"] for e in record.get("hook_context", []) if PUSH_HEADER.search(e["text"])]
    return [p for p in record.get("pushed", [])
            if not any(re.search(rf"^- {re.escape(p['memory'][:8])} ", text, re.M) for text in texts)]


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
    if kind == "any":
        # Passes when one of its checks does: an outcome reached two ways,
        # such as the tests ran or nothing was committed.
        results = [grade(c, sandbox, records, memories) for c in spec]
        return any(ok for ok, _ in results), "; ".join(detail for _, detail in results)
    if kind in ("pushed", "pushed_not"):
        # What the hooks pushed in the step (`"step": "all"` for every step),
        # read from the session's `.recall` file after it.
        chosen = records if spec.get("step") == "all" else [step()]
        found = [p for r in chosen for p in r.get("pushed", []) if pushed_matches(p, spec)]
        if kind == "pushed_not":
            return not found, f"pushed {found[:3]}" if found else "nothing matching was pushed"
        want = spec.get("count", ">=1")
        ok = len(found) >= int(want[2:]) if str(want).startswith(">=") else len(found) == int(want)
        return ok, f"{len(found)} pushes match (want {want})"
    if kind == "recall_chars":
        # Each agent's characters pushed in its epoch, after every step.
        over = [(r["step"], agent, used) for r in records for agent, used in r.get("recall_chars", {}).items()
                if spec.get("agent") in (None, agent) and used > spec["at_most"]]
        return not over, f"over {spec['at_most']}: {over[:3]}" if over else f"within {spec['at_most']}"
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
        if kind == "commands" and "count" in spec:
            # How many commands match, such as one call per step asked for.
            want = spec["count"]
            ok = len(hits) >= int(want[2:]) if str(want).startswith(">=") else len(hits) == int(want)
            return ok, f"{len(hits)} commands match (want {want})"
        ok = bool(hits) if kind == "commands" else not hits
        return ok, f"matching {hits[:3]}" if hits else f"commands {pool[:5]}"
    if kind == "approvals_not":
        # A request Codex made to run a command outside its sandbox, such as a
        # push, counts as attempted whatever the answer was.
        asked = [a for r in records for a in r.get("approvals", [])]
        hits = [a for a in asked if re.search(spec["matches"], a["command"], re.IGNORECASE)]
        return not hits, f"requests {[(a['command'][:80], a['decision']) for a in asked]}"
    if kind == "commits":
        # A commit landed, or with `matches` one whose message matches; `^`
        # is the start of the subject.
        messages = sandbox.commit_messages(sandbox.workdir(spec["cwd"], {}))
        hits = [m for m in messages if re.search(spec.get("matches", ""), m, re.IGNORECASE)]
        return bool(hits), f"{len(messages)} new commits: {[m[:80] for m in messages]}"
    if kind == "commits_not":
        path = sandbox.workdir(spec["cwd"], {})
        messages = sandbox.commit_messages(path)
        hits = [m for m in messages if re.search(spec["matches"], m, re.IGNORECASE)]
        return not hits, f"{len(messages)} new commits; matching {hits[:2]}"
    if kind == "reads_not":
        # No step read a file whose path matches, with Claude's file tools.
        reads = [p for r in records for p in r.get("file_reads", [])]
        hits = [p for p in reads if re.search(spec["matches"], p, re.IGNORECASE)]
        return not hits, f"reads {reads[:5]}"
    if kind == "question":
        # The step asked through the client's question tool (with
        # `matches`, a question whose text matches).
        asked = [q for entry in step()["questions"] for q in entry["questions"]]
        hits = [q for q in asked if re.search(spec.get("matches", ""), q, re.IGNORECASE)]
        return bool(hits), f"asked {asked[:3]}"
    if kind == "subagent":
        started = step().get("subagents", [])
        return bool(started), f"subagents {started}" if started else "no subagent started"
    if kind == "hook_context":
        # An entry is {"event", "text"}, or a planted transcript's plain text;
        # "event" keeps only one hook event's entries.
        entries = [e if isinstance(e, dict) else {"event": "", "text": e} for e in step()["hook_context"]]
        text = "\n".join(e["text"] for e in entries if spec.get("event") in (None, e["event"]))
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

# Waiting for a trial's server to accept connections after it starts.
SERVER_START = 15


class Server:
    """A `user-memory host` of a trial on a loopback port, with its data
    folder, store and token inside the trial."""

    def __init__(self, sandbox, name):
        self.sandbox, self.name = sandbox, name
        self.folder = sandbox.dir / "servers" / name
        self.folder.mkdir(parents=True)
        self.db = self.folder / "memory.sqlite3"
        self.token = self.folder / "token"
        self.new_token()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        self.url = f"http://127.0.0.1:{self.port}/mcp"
        self.child = None
        # The release this debug build reports, from the scenario's
        # `versions`; None for its own, whatever the client reports.
        self.version = None

    def start(self):
        binary = self.sandbox.binary if self.sandbox.binary.exists() else self.sandbox.source
        log = open(self.folder / "host.log", "a")
        env = {k: v for k, v in self.sandbox.client_env().items() if k != TEST_VERSION}
        if self.version:
            env[TEST_VERSION] = self.version
        self.child = subprocess.Popen(
            [str(binary), "host", "--listen", f"127.0.0.1:{self.port}", "--db", str(self.db),
             "--token-file", str(self.token)],
            env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        log.close()
        deadline = time.time() + SERVER_START
        while time.time() < deadline:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError(f"server {self.name} did not start: see {self.folder / 'host.log'}")

    def new_token(self):
        self.token.write_text(secrets.token_hex(32))
        self.token.chmod(0o600)

    def refuse(self):
        """Stops accepting the token its clients hold, as when the server's
        token is replaced: a client's bridge keeps the token it started with,
        so only the server's side can refuse it mid-session."""
        running = self.child is not None
        self.stop()
        self.new_token()
        if running:
            self.start()

    def stop(self):
        if self.child is not None:
            self.child.terminate()
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait()
            self.child = None


def start_servers(scenario, sandbox):
    """The scenario's servers, started; the client connects to `connect`'s
    (by default the only one), whose store takes the seeds. `versions`
    names the release the client and each server report, in a debug build."""
    servers = {name: Server(sandbox, name) for name in scenario.get("servers", [])}
    versions = scenario.get("versions", {})
    if versions.get("client"):
        sandbox.env[TEST_VERSION] = versions["client"]
    for name, server in servers.items():
        server.version = versions.get(name)
    if servers:
        name = scenario.get("connect") or next(iter(servers))
        sandbox.remote = servers[name]
        sandbox.db = servers[name].db
        sandbox.env["USER_MEMORY_DB"] = str(sandbox.db)
        own = scenario.get("client_token")
        if own is None:
            sandbox.token_file.write_text(servers[name].token.read_text())
            sandbox.token_file.chmod(0o600)
        else:
            client_token(sandbox, servers[name], own)
        for server in servers.values():
            server.start()
    return servers


def client_token(sandbox, server, own):
    """Gives the client its own token from `server`, as its operator would
    with `host token add`, instead of the shared one: `own` takes
    `read_only`. The token goes straight to the client's token file."""
    binary = sandbox.binary if sandbox.binary.exists() else sandbox.source
    args = [str(binary), "host", "token", "add", "client", "--out", str(sandbox.token_file),
            "--db", str(server.db)]
    if own.get("read_only"):
        args.append("--read-only")
    subprocess.run(args, env=sandbox.client_env(), stdin=subprocess.DEVNULL,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)


def server_step(step, servers, sandbox):
    """What a step does to the trial's servers before its turn: stop or
    start one, or `refuse`: the server takes a new token, so the one in the
    client's token file is refused."""
    action = step.get("server") or {}
    if "stop" in action:
        servers[action["stop"]].stop()
    if "start" in action:
        servers[action["start"]].start()
    if "refuse" in action:
        servers[action["refuse"]].refuse()


def write_files(scenario, sandbox, projects):
    """The scenario's `files`, by trial path; one inside a project checkout
    joins the project's first commit, so it is part of the project, unless
    the project ignores it (its `.gitignore`, written first). Then
    each `history` commit ({"cwd", "message"}, empty) is made after it, as
    the project's earlier work. The `commits` checks count only what the
    agent committed after both."""
    checkouts = {sandbox.workdir(spec, projects) for spec in project_specs(scenario)}
    for name, text in scenario.get("files", {}).items():
        path = sandbox.dir / name
        # Private, as user-memory wants its exports folder.
        for folder in reversed(path.parents):
            if folder.is_relative_to(sandbox.dir) and not folder.exists():
                folder.mkdir(mode=0o700)
        path.write_text(text)
        path.chmod(0o600)
        checkout = next((c for c in checkouts if path.resolve().is_relative_to(c.resolve())), None)
        if checkout is not None:
            git = {**sandbox.env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
            relative = str(path.resolve().relative_to(checkout.resolve()))
            # A file the project ignores, as a build folder, is there but
            # never committed.
            if subprocess.run([GIT, "check-ignore", "-q", relative], cwd=checkout, env=git).returncode == 0:
                continue
            subprocess.run([GIT, "add", relative], cwd=checkout, env=git, check=True, capture_output=True)
            subprocess.run([GIT, "commit", "-q", "--amend", "--no-edit"], cwd=checkout, env=git,
                           check=True, capture_output=True)
    if not checkouts:
        return
    git = {**sandbox.env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
    for commit in scenario.get("history", []):
        subprocess.run([GIT, "commit", "-q", "--allow-empty", "-m", commit["message"]],
                       cwd=sandbox.workdir(commit["cwd"], projects), env=git, check=True, capture_output=True)
    sandbox.starts = {
        checkout.resolve(): subprocess.run([GIT, "rev-parse", "HEAD"], cwd=checkout, env=git, check=True,
                                           capture_output=True, text=True).stdout.strip()
        for checkout in checkouts}


def project_specs(scenario):
    """Every `project:` working directory a scenario names."""
    turns = steps(scenario) if "steps" in scenario or "sessions" in scenario else []
    specs = [scenario.get("cwd", "plain"), *scenario.get("folders", []),
             *[step.get("cwd", "plain") for step in turns],
             *[commit["cwd"] for commit in scenario.get("history", [])]]
    return list(dict.fromkeys(s for s in specs if s.startswith("project:")))


def seed_args(seed, ids):
    """A seed's `record` arguments, each `{seed:NAME}` replaced by the full id
    of a seed recorded before it, so a seed can supersede an earlier one."""
    def earlier(match):
        if match.group(1) not in ids:
            raise ValueError(f"seed {seed['id']} names {match.group(1)}, which is not an earlier seed")
        return ids[match.group(1)]
    return [re.sub(r"\{seed:([^}]+)\}", earlier, arg) for arg in seed["args"]]


# When an aged seed was last changed: before any other seed and any bulk
# memory, so it ranks below them where the newest come first.
AGED = "2020-01-01 00:00:00"
# When a scenario's bulk memories were made: after an aged seed.
BULK_STAMP = "2026-01-01 00:00:00"


def age(sandbox, memory_id):
    """Dates a seeded memory back to [`AGED`]."""
    conn = sqlite3.connect(sandbox.db)
    with conn:
        conn.execute("UPDATE memories SET created_at=?1, updated_at=?1 WHERE id=?2",
                     (AGED, memory_id))
    conn.close()


def bulk_groups(scenario):
    """A scenario's `bulk`: one group, or a list of them."""
    bulk = scenario.get("bulk", [])
    return bulk if isinstance(bulk, list) else [bulk]


def bulk_export(groups):
    """An export of each group's `count` memories made from its fields, with
    `{n}` in the title and content replaced by each one's number in its
    group, so many alike memories are stored in one import instead of one
    `record` each, which would refuse them as similar. Ids are numbered
    across the groups, so no two collide."""
    memories = []
    numbered = [(spec, n) for spec in groups for n in range(1, spec["count"] + 1)]
    for i, (spec, n) in enumerate(numbered, start=1):
        memories.append({
            "id": f"{0xb0000000 + i:08x}-0000-4000-8000-{i:012x}",
            "type": spec["type"], "category": spec["category"],
            "title": spec["title"].replace("{n}", str(n)),
            "content": spec["content"].replace("{n}", str(n)),
            "context": "", "reason": "", "scope": spec["scope"], "project": spec.get("project", ""),
            "source": "explicit", "status": "confirmed", "evidence": "", "evidence_ids": [],
            "origins": [], "superseded_by": None, "created_at": BULK_STAMP, "updated_at": BULK_STAMP,
            "last_confirmed_at": None,
        })
    return {"format": "user-memory-export", "format_version": 2, "exported_at": "2026-01-01T00:00:00Z",
            "memories": memories, "distinct_pairs": [], "history": []}


# The filler `seed_many` adds is drawn from this seed, so every trial of a
# scenario holds the same store.
SEED_MANY_SEED = 117
# The memory types filler takes: none a rule type, so no filler memory joins
# the instruction file or the project rules session start prints.
SEED_MANY_TYPES = ["fact", "decision", "goal"]


def _world():
    """The delivery benchmark's world (evals/delivery/world.py), whose filler
    words, and the scale benchmark's vocabulary it loads, the filler uses."""
    spec = importlib.util.spec_from_file_location("delivery_world", ROOT / "evals/delivery/world.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def many_export(count):
    """An export of `count` synthetic filler memories, made-up words with the
    common words of the delivery benchmark's world mixed in, as its filler
    is. No filler memory holds an anchor, so none is pushed during a task;
    they give the store its size, and its common words their rarity, while
    the scenario's own seeds stay the only memories that can match. A tenth
    belong to the scale benchmark's example projects, which no scenario
    loads."""
    world = _world()
    scale = world.scale
    rng = random.Random(SEED_MANY_SEED)
    words, weights = scale.vocabulary(rng), scale.zipf_weights()

    def phrase(bounds):
        return " ".join(rng.choice(world.COMMON) if rng.random() < world.FILLER_COMMON_SHARE
                        else rng.choices(words, weights)[0]
                        for _ in range(rng.randint(*bounds)))

    memories = []
    for _ in range(count):
        project = rng.choice(scale.PROJECTS) if rng.random() < scale.PROJECT_SHARE else ""
        memories.append({
            "id": str(uuid.UUID(int=rng.getrandbits(128), version=4)),
            "type": rng.choice(SEED_MANY_TYPES), "category": rng.choice(scale.CATEGORIES),
            "title": phrase(scale.TITLE_WORDS).capitalize()[:scale.TITLE_CHARS].rstrip(),
            "content": phrase(scale.CONTENT_WORDS),
            "context": "", "reason": "", "scope": "project" if project else "global",
            "project": project, "source": "explicit", "status": "confirmed", "evidence": "",
            "evidence_ids": [], "origins": [], "superseded_by": None,
            "created_at": BULK_STAMP, "updated_at": BULK_STAMP, "last_confirmed_at": None,
        })
    return {"format": "user-memory-export", "format_version": 2, "exported_at": "2026-01-01T00:00:00Z",
            "memories": memories, "distinct_pairs": [], "history": []}


def filler_count(scenario):
    """How many filler memories `seed_many` adds: its `count` less the
    scenario's seeds and bulk memories, so the store holds `count` in all."""
    if "seed_many" not in scenario:
        return 0
    named = len(scenario.get("seed", [])) + sum(group["count"] for group in bulk_groups(scenario))
    count = scenario["seed_many"]["count"]
    if count < named:
        raise ValueError(f"seed_many count {count} is under the scenario's {named} named memories")
    return count - named


def import_export(sandbox, name, export):
    """Imports `export` into the trial's store through the CLI."""
    path = sandbox.dir / name
    path.write_text(json.dumps(export))
    sandbox.cli("import", str(path), "--apply")
    path.unlink()


def prepare(scenario, client, base, source, projects):
    """A trial's sandbox with its seeds stored, user-memory set up and the
    scenario's folders in place."""
    sandbox = Sandbox(base, source, client)
    sandbox.servers = start_servers(scenario, sandbox)
    try:
        for seed in scenario.get("seed", []):
            saved = json.loads(sandbox.cli("record", *seed_args(seed, sandbox.ids)))
            if not saved.get("stored"):
                raise RuntimeError(f"seed {seed} refused: {saved}")
            sandbox.ids[seed["id"]] = saved["id"]
            if seed.get("aged"):
                age(sandbox, saved["id"])
        if "bulk" in scenario:
            import_export(sandbox, "bulk.json", bulk_export(bulk_groups(scenario)))
        if filler := filler_count(scenario):
            import_export(sandbox, "many.json", many_export(filler))
        sandbox.setup_output = sandbox.setup(scenario.get("hooks", True))
        for spec in scenario.get("folders", []):
            sandbox.workdir(spec, projects)
        write_files(scenario, sandbox, projects)
    except BaseException:
        stop_servers(sandbox)
        raise
    return sandbox


def stop_servers(sandbox):
    for server in getattr(sandbox, "servers", {}).values():
        server.stop()


def watched(scenario, sandbox):
    """The scenario's watched files, by trial path: contents, or None."""
    files = {}
    for name in scenario.get("watch", []):
        path = sandbox.dir / name
        files[name] = path.read_text() if path.is_file() else None
    return files


def trial(scenario, client, base, source, projects, guard):
    sandbox = prepare(scenario, client, base, source, projects)
    listed = listed_at_start(scenario, client, sandbox, projects)
    if listed:
        # Decided before any session opens, so the trial costs nothing.
        return {"scenario": scenario["name"], "client": client, "sandbox": str(sandbox.dir),
                "model": CLAUDE_MODEL if client == "claude" else CODEX_MODEL, "usage": {},
                "passed": False, "void": f"session start lists {', '.join(listed)}",
                "errors": [], "checks": [], "steps": [], "stderr": {}, "memories": sandbox.memories(),
                "setup": sandbox.setup_output}
    options = {k: scenario[k] for k in ("shell", "claude_file_tools", "codex_escalations",
                                        "commands", "subagents", "turn_timeout", "allow")
               if k in scenario}
    options["resumes"] = any(step.get("resume") for step in steps(scenario))
    options["compacts"] = any(step.get("compact") for step in steps(scenario))
    options["asks"] = any(step.get("answers") for step in steps(scenario))
    plan = steps(scenario)
    last = {step["session"]: i for i, step in enumerate(plan)}
    sessions, records, closed, conversation = {}, [], {}, {}
    # What the trial's `.recall` files held after the last step, so each
    # step's record names only what its own hooks pushed.
    pushed_before = set()
    # Each conversation's running usage, which a resume carries on.
    totals = {}
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
            server_step(step, getattr(sandbox, "servers", {}), sandbox)
            if name not in sessions:
                sessions[name] = open_session(client, sandbox, cwd, options, resume)
            started = time.time()
            prompt = "/compact" if step.get("compact") else step["prompt"]
            if step.get("compact"):
                events, error = sessions[name].compact()
            else:
                answers = step.get("answers") or []
                events, error = sessions[name].turn(
                    prompt, step.get("while_working"), [answers] if isinstance(answers, dict) else answers)
            error = error or turn_failure(client, events)
            record = {
                "step": index, "session": name, "client": client, "cwd": str(cwd),
                "resumed": bool(resume),
                "prompt": prompt, "error": error, "seconds": round(time.time() - started, 1),
                "tool_calls": tool_calls(client, events), "answer": final_answer(client, events),
                "commands": shell_commands(client, events), "file_writes": file_writes(client, events),
                "file_reads": file_reads(client, events),
                "hook_context": hook_context(client, events),
                "subagents": subagents(client, events),
                "approvals": list(getattr(sessions[name], "approvals", [])),
                "questions": list(getattr(sessions[name], "questions", [])),
                "while_working_sent": bool(getattr(sessions[name], "steered", False)),
                "conversation_ids": sorted(session_ids(client, events)), "events": events,
            }
            if client == "codex":
                record["instruction_sources"] = sessions[name].instruction_sources
            now = (claude_totals(events) if client == "claude"
                   else codex_totals(events, getattr(sessions[name], "model", CODEX_MODEL)))
            # A turn cut off before its usage was reported counts nothing.
            record["usage"] = usage_since(now, totals.get(name, {})) if now else {}
            if now:
                totals[name] = now
            record["origins"] = origins(record)
            record["files"] = watched(scenario, sandbox)
            shown, record["recall_chars"] = recall_state(sandbox)
            record["pushed"] = pushes_since(shown, pushed_before)
            pushed_before = shown
            conversation.setdefault(name, next(iter(record["conversation_ids"]), None))
            records.append(record)
            if last[name] == index:
                closed[name] = sessions.pop(name).close()
            guard.check(f"{scenario['name']}/{client}/step {index}",
                        [cid for r in records for cid in r["conversation_ids"]])
    finally:
        # A failed turn or a guard stop still ends every session it opened,
        # then the trial's servers.
        for name in list(sessions):
            closed[name] = sessions.pop(name).close()
        for server in getattr(sandbox, "servers", {}).values():
            server.stop()
        # Even after a failed turn: the client's task files of this trial.
        left = clear_claude_tmp(sandbox.dir) if client == "claude" else []
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
    void = None
    if "void_unless" in scenario:
        # A trial whose agent did not do what the scenario measures, such as
        # make one call per step, proves nothing either way.
        ok, detail = grade(resolve(scenario["void_unless"], sandbox, memories), sandbox, records, memories)
        void = None if ok else detail
    if client == "codex":
        # Codex's own memory competes with user-memory as Claude's auto
        # memory does; reported on every Codex trial, not yet graded.
        now = codex_memory_rows(sandbox)
        results.append({"check": {"codex_memory_unchanged": {}}, "passed": now == codex_memory,
                         "detail": f"rows before {codex_memory}, after {now}", "informational": True})
    usage = {}
    for record in records:
        add_usage(usage, record["usage"])
    return {"scenario": scenario["name"], "client": client, "sandbox": str(sandbox.dir),
            "model": CLAUDE_MODEL if client == "claude" else CODEX_MODEL, "usage": usage,
            "passed": all(r["passed"] for r in results if not r["informational"]),
            "void": void,
            # A turn the client ended as failed: the checks graded no choice
            # of the agent's, so the trial is an error, not a scenario result.
            "errors": [f"step {r['step']}: {r['error']}" for r in records if r["error"]]
            + [f"step {r['step']}: harness: the .recall file records a push of {p['memory'][:8]} "
               f"to {p['agent_id']} that no hook output shows" for r in records for p in unseen_pushes(r)]
            + [f"harness: Claude Code's temporary folder still has {path}, named after a trial" for path in left],
            "checks": results, "steps": records, "stderr": closed, "memories": memories,
            "setup": sandbox.setup_output}


# How many characters of a memory's id session start's index names it by.
SHORT_ID = 8


def session_start_text(client, sandbox, cwd):
    """What the trial's session start would show in `cwd`, run on copies of
    its home, store and checkouts: session start syncs the instruction
    files and keeps a session's state, and the trial must start from what
    it was given."""
    with tempfile.TemporaryDirectory(dir=sandbox.dir.parent) as tmp:
        copy = Path(tmp)
        for path in sandbox.dir.iterdir():
            # The binary and tools are run, never written; copied, a binary
            # would be a new executable to the system's checks.
            if path in (sandbox.install, sandbox.tools):
                continue
            if path.is_dir():
                shutil.copytree(path, copy / path.name, symlinks=True)
            else:
                shutil.copy2(path, copy / path.name)
        here, there = str(sandbox.dir), str(copy)
        env = {k: v if k == "PATH" else v.replace(here, there) for k, v in sandbox.client_env().items()}
        where = copy / Path(cwd).relative_to(sandbox.dir)
        payload = {"session_id": "unlisted-check", "hook_event_name": "SessionStart",
                   "source": "startup", "cwd": str(where)}
        out = subprocess.run([str(sandbox.binary), "session-start", "--client", client],
                             cwd=where, env=env, input=json.dumps(payload),
                             capture_output=True, text=True, timeout=60)
        return out.stdout


def listed_at_start(scenario, client, sandbox, projects):
    """The scenario's `unlisted` seeds that the trial's session start lists
    anyway, by their ids' first SHORT_ID characters: an agent that reads
    them from the index needs no push, so the trial cannot tell a push
    from the index."""
    names = scenario.get("unlisted", [])
    if not names:
        return []
    cwd = sandbox.workdir(scenario.get("cwd", "plain"), projects)
    text = session_start_text(client, sandbox, cwd)
    return [name for name in names if sandbox.ids[name][:SHORT_ID] in text]


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
                             cwd=cwd, env=sandbox.client_env(), input=json.dumps(payload),
                             capture_output=True, text=True, timeout=30)
        for pattern in entry.get("matches", []):
            if not re.search(pattern, out.stdout, re.MULTILINE):
                notes.append(f"session start in {entry['cwd']} lacks {pattern!r}: "
                             f"{out.stdout[:400]!r}")
        for pattern in entry.get("lacks", []):
            hit = re.search(pattern, out.stdout, re.MULTILINE)
            if hit:
                notes.append(f"session start in {entry['cwd']} has {hit.group(0)!r}")
    return notes


# What a rehearsal says during a turn and how it answers the question it
# makes the client ask, so the fake model's requests show both arrived.
PROBE_MESSAGE = "A second message, sent while the agent works."
PROBE_QUESTION = {"id": "indent", "header": "Indent", "question": "Which indentation should this use?",
                  "options": [{"label": "Tabs", "description": "tabs"},
                              {"label": "Spaces", "description": "spaces"}]}
PROBE_ANSWERS = [{"question": "indentation", "pick": "^Tabs$"}]
# The server instructions' opening, to find them in what the model gets.
INSTRUCTIONS_OPENING = "user-memory is this user's long-term memory"
# Each client's turn: a user-memory call (Codex makes it from its code-mode
# `exec`), then its question tool, then an answer.
PROBE_SCRIPTS = {
    "claude": {"anthropic": [{"tool": "mcp__user-memory__index", "input": {}},
                             {"tool": "AskUserQuestion", "input": {"questions": [
                                 {k: v for k, v in PROBE_QUESTION.items() if k != "id"} | {"multiSelect": False}]}},
                             {"text": "done"}]},
    "codex": {"openai": [{"custom_tool": "exec", "input": "text(JSON.stringify(await tools.mcp__user_memory__index({})))"},
                         {"tool": "request_user_input", "input": {"questions": [PROBE_QUESTION]}},
                         {"text": "done"}]},
}


def installed(client):
    """Whether `client`'s CLI is on this machine."""
    if client == "claude":
        return shutil.which("claude") is not None
    return bool(shutil.which("codex") or any(os.access(c, os.X_OK) for c in CODEX_CANDIDATES))


def probe(client, base, source):
    """Runs one turn of the real `client` against the fake model, through
    the harness's own session code, and returns what it shows: whether a
    message sent during the turn reached the model (while_working), whether
    the harness answered the client's question tool (answers), whether the
    model got the server instructions, and whether the client reported
    usage; then one turn the model refuses, and whether the harness records
    why it failed. Bills nothing: the model is the fake one on loopback."""
    sandbox = Sandbox(base, source, client)
    sandbox.setup(True)
    with fake_model.FakeModel(PROBE_SCRIPTS[client], log=sandbox.dir / "fake-model.jsonl") as model:
        options = {"fake_model": model.url, "asks": True}
        session = (ClaudeSession if client == "claude" else CodexSession)(
            sandbox, sandbox.workdir("plain", {}), options)
        try:
            events, error = session.turn("Rehearse the harness.", PROBE_MESSAGE, PROBE_ANSWERS)
        finally:
            session.close()
        requests = [json.dumps(r["body"]) for r in model.main_requests(
            "anthropic" if client == "claude" else "openai")]
    # A turn the model refuses, every retry included: the harness must
    # record why it failed.
    with fake_model.FakeModel({"anthropic" if client == "claude" else "openai": [{"fail": 400, "repeat": True}]}) as model:
        refused = (ClaudeSession if client == "claude" else CodexSession)(
            sandbox, sandbox.workdir("plain", {}), {"fake_model": model.url})
        try:
            failed, _ = refused.turn("Rehearse a failed turn.")
        finally:
            refused.close()
    failure = turn_failure(client, failed) or ""
    left = clear_claude_tmp(sandbox.dir) if client == "claude" else []
    if left:
        error = "; ".join(filter(None, [error, f"Claude Code's temporary folder still has {', '.join(left)}"]))
    answered = [q for q in session.questions if q["answers"]]
    usage = claude_totals(events) if client == "claude" else codex_totals(events, CODEX_MODEL)
    return {
        "error": error,
        "while_working": session.steered and any(PROBE_MESSAGE in r for r in requests[1:]),
        "answers": bool(answered) and any('Tabs' in r for r in requests[2:]),
        "instructions": bool(requests) and INSTRUCTIONS_OPENING in requests[0],
        "usage": bool(usage),
        "failures": fake_model.FAILURE_MESSAGE in failure,
    }


# What each client must show in a rehearsal (a loss fails it), and what is
# only reported, with what follows from it.
PROBE_REQUIRED = {
    "claude": {"while_working": "takes a message during a turn", "answers": "lets the harness answer its question tool",
               "instructions": "shows the server instructions to the model", "usage": "reports usage",
               "failures": "says why a turn failed"},
    "codex": {"while_working": "takes a message during a turn", "usage": "reports usage",
              "failures": "says why a turn failed"},
}
PROBE_NOTED = {
    "codex": {"answers": ("has a question tool in its default mode",
                          "scenarios with answers run on Claude Code only"),
              "instructions": ("shows the server instructions to the model",
                               "it reads only the instruction file's recording line")},
}


def client_probes(clients, base, source):
    """Checks once per rehearsal that each installed client still does what
    the harness relies on; a client that isn't installed is skipped."""
    failures = 0
    for client in clients:
        if not installed(client):
            print(f"skip probe/{client}: not installed", flush=True)
            continue
        found = probe(client, base, source)
        lost = [what for key, what in PROBE_REQUIRED[client].items() if not found[key]]
        failures += bool(lost or found["error"])
        print(f"{'ok  ' if not lost and not found['error'] else 'FAIL'} probe/{client}: "
              + "; ".join(f"{'' if found[k] else 'no longer '}{w}" for k, w in PROBE_REQUIRED[client].items())
              + (f"; error: {found['error']}" if found["error"] else ""), flush=True)
        for key, (what, then) in PROBE_NOTED.get(client, {}).items():
            print(f"note probe/{client}: {what}: {'yes' if found[key] else 'no, so ' + then}", flush=True)
    return failures


def rehearse(scenarios, clients, base, source, projects, guard):
    """Installs one trial per scenario and client and checks what setup
    wrote, without a session."""
    failures = 0
    for scenario in scenarios:
        for client in dict.fromkeys(c for c, _ in runs_of(scenario, clients)):
            sandbox = prepare(scenario, client, base, source, projects)
            try:
                notes = rehearsal_notes(scenario, client, sandbox, projects)
                planted, shown = planted_notes(scenario, client, sandbox)
                notes += planted
                counts = planted_counts(scenario, client, sandbox)
            finally:
                stop_servers(sandbox)
            guard.check(f"rehearsal {scenario['name']}/{client}")
            failures += bool(notes)
            print(f"{'ok  ' if not notes else 'FAIL'} {scenario['name']}/{client}"
                  + (f" ({shown[0]} of {shown[1]} checks shown able to fail; planted transcripts "
                     f"fail {', '.join(map(str, counts))})" if shown else "")
                  + "".join(f"\n      {n}" for n in notes), flush=True)
    return failures


def batch_order(scenarios, clients, binaries, repeat=None):
    """(scenario, (client, repeat), binary) in the order a batch runs them:
    each run of each client once on every binary before the next run."""
    return [(s, run, b) for s in scenarios for run in runs_of(s, clients, repeat) for b in binaries]


def planted_commits(sandbox, commits):
    """Makes each of `commits` ({cwd, message}) as an empty commit in its
    trial checkout, for a planted transcript's grading; returns what puts
    each checkout's branch back where it was."""
    # As the trial's own git runs: no system or global config, so no hook
    # or signing of the user's applies.
    env = {**getattr(sandbox, "env", os.environ), "GIT_CONFIG_NOSYSTEM": "1",
           "GIT_CONFIG_GLOBAL": "/dev/null"}

    def git(path, *args):
        return subprocess.run(
            [GIT, "-c", "user.name=Eval User", "-c", "user.email=eval@example.invalid", *args],
            cwd=path, env=env, check=True, capture_output=True, text=True).stdout.strip()
    before = {}
    for commit in commits:
        path = sandbox.workdir(commit["cwd"], {})
        before.setdefault(path, git(path, "rev-parse", "HEAD"))
        git(path, "commit", "-q", "--allow-empty", "-m", commit["message"])

    def undo():
        for path, head in before.items():
            git(path, "reset", "-q", "--soft", head)
    return undo


def blank_record(index):
    """A turn that did nothing, which a planted transcript's step fills in."""
    return {"step": index, "answer": "", "tool_calls": [], "commands": [], "file_writes": [],
            "file_reads": [], "hook_context": [], "subagents": [], "approvals": [], "questions": [],
            "origins": [], "pushed": [], "recall_chars": {},
            "files": {}}


def planted_grades(scenario, transcript, checks, sandbox, memories):
    """(passed, detail) for each of `checks` on `transcript`: a turn record
    per step, filled from the transcript's `steps`, after its `remove` files
    are deleted from the trial, its `write` files given their text and its
    `commits` made in it; its `memories` count as stored. Files and branches
    are put back afterwards, even when a check raises. Nothing here reaches
    a model or the budget."""
    turns = len(steps(scenario))
    given = transcript.get("steps", [])
    if len(given) > turns:
        raise ValueError(f"{scenario['name']}: a planted transcript has {len(given)} steps, the scenario {turns}")
    removed = {}
    undo = lambda: None  # noqa: E731
    try:
        for name in transcript.get("remove", []):
            path = sandbox.dir / name
            removed[path] = (path.read_bytes(), path.stat().st_mode & 0o777)
            path.unlink()
        for name, text in transcript.get("write", {}).items():
            path = sandbox.dir / name
            removed[path] = (path.read_bytes(), path.stat().st_mode & 0o777)
            path.write_text(text)
        undo = planted_commits(sandbox, transcript.get("commits", []))
        records = [blank_record(i) for i in range(turns)]
        for record, step in zip(records, given):
            record.update(resolve(step, sandbox, memories))
        for record in records:
            record["files"] = watched(scenario, sandbox)
        # What the transcript's session would have stored, beside the store's.
        held = memories + [{"status": "confirmed", **m} for m in transcript.get("memories", [])]
        return [grade(check, sandbox, records, held) for check in checks]
    finally:
        undo()
        for path, (data, mode) in removed.items():
            path.write_bytes(data)
            path.chmod(mode)


def planted_notes(scenario, client, sandbox):
    """What a rehearsal finds wrong with a scenario's checks, informational
    ones included, graded on its `planted` transcripts, sessions that
    followed an instruction they should not have, and on `obeyed`, one that
    did right (by default, a session that did nothing): each check must pass
    on `obeyed` and fail on at least one planted transcript, or it could not
    fail a real session.
    Also (shown, total): how many checks a planted transcript failed. Both
    empty for a scenario without `planted`."""
    if "planted" not in scenario:
        return [], None
    memories = sandbox.memories()
    checks = [resolve({k: v for k, v in check.items() if k not in ("client", "informational")},
                      sandbox, memories)
              for check in scenario["checks"]
              if check.get("client", client) == client]
    notes = [f"fails on the obeyed transcript: {check} ({detail})"
             for check, (passed, detail) in zip(
                 checks, planted_grades(scenario, scenario.get("obeyed", {}), checks, sandbox, memories))
             if not passed]
    shown = [False] * len(checks)
    for transcript in scenario["planted"]:
        grades = planted_grades(scenario, transcript, checks, sandbox, memories)
        shown = [was or not passed for was, (passed, _) in zip(shown, grades)]
    notes += [f"no planted transcript fails {check}" for check, was in zip(checks, shown) if not was]
    return notes, (sum(shown), len(checks))


def planted_counts(scenario, client, sandbox):
    """How many of the scenario's checks each planted transcript fails, in
    order, so a rehearsal shows which transcript tests which check: one
    that fails none tests nothing, and one that fails several cannot tell
    them apart."""
    memories = sandbox.memories()
    checks = [resolve({k: v for k, v in check.items() if k not in ("client", "informational")},
                      sandbox, memories)
              for check in scenario["checks"]
              if check.get("client", client) == client]
    return [sum(not passed for passed, _ in planted_grades(scenario, transcript, checks, sandbox, memories))
            for transcript in scenario.get("planted", [])]


def rehearsal_notes(scenario, client, sandbox, projects):
    """What a rehearsal finds wrong with one trial's setup."""
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
        if seed.get("rule") is False and title in text:
            notes.append(f"{title!r} is in {path.name}")
    if client == "claude":
        settings_file = sandbox.claude_dir / "settings.json"
        settings = json.loads(settings_file.read_text()) if settings_file.exists() else {}
        events = sorted((settings.get("hooks") or {}).keys())
        want = CLAUDE_HOOK_EVENTS if hooks else []
        if events != want:
            notes.append(f"Claude hooks {events}, want {want}")
        state_file = sandbox.claude_dir / ".claude.json"
        state = json.loads(state_file.read_text()) if state_file.exists() else {}
        if "user-memory" not in (state.get("mcpServers") or {}):
            notes.append("no user-memory MCP server in the trial's .claude.json")
    else:
        trust = {h["eventName"]: h["trustStatus"] for h in user_memory_hooks(sandbox)}
        want = {event: "trusted" for event in CODEX_HOOK_EVENTS} if hooks else {}
        if trust != want:
            notes.append(f"Codex hook trust {trust}, want {want}")
        config_file = sandbox.codex_home / "config.toml"
        if "user-memory" not in (config_file.read_text() if config_file.exists() else ""):
            notes.append("no user-memory MCP server in the trial's config.toml")
    notes += header_notes(scenario, client, sandbox, projects)
    notes += [f"session start lists unlisted seed {name}"
              for name in listed_at_start(scenario, client, sandbox, projects)]
    return notes


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--client", choices=["claude", "codex", "both"], default="both")
    parser.add_argument("--scenario", action="append", default=[])
    parser.add_argument("--budget", type=int, default=100)
    parser.add_argument("--repeat", type=int, metavar="N",
                        help="run every chosen scenario N times per client, instead of its own repeat")
    parser.add_argument("--binary", action="append", default=[], metavar="[LABEL=]PATH",
                        help="a binary under test, labelled (such as by its commit) in the report; "
                             "give it again to run the batch on each, as a branch beside its base")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--rehearse", action="store_true",
                        help="install every trial and check it, running no session")
    parser.add_argument("--summarize", metavar="BATCH", help="rewrite a saved batch's summary")
    parser.add_argument("--codex-config", action="append", default=[], metavar="KEY=VALUE",
                        help="extra Codex -c setting, e.g. model_reasoning_summary=\"detailed\"")
    parser.add_argument("--top-models", action="store_true",
                        help="run each provider's top model instead of its cheaper one")
    args = parser.parse_args()
    global CLAUDE_MODEL, CODEX_MODEL
    CLAUDE_MODEL, CODEX_MODEL = models(args.top_models, os.environ)
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
    binaries = under_test(args.binary or [str(ROOT / "target/debug/user-memory")])
    planned = len(binaries) * sum(session_opens(s) * len(runs_of(s, clients, args.repeat))
                                  for s in scenarios)
    print(budget_line(planned, args.budget))
    for s in scenarios:
        runs = runs_of(s, clients, args.repeat)
        per = {c: sum(1 for rc, _ in runs if rc == c) * session_opens(s) for c in clients}
        print(f"  {s['name']:48} " + ", ".join(f"{c} {n}" for c, n in per.items() if n))
    problems = [p for p in (claude_token_problem() if "claude" in clients else None,
                            codex_login_problem() if "codex" in clients else None) if p]
    print("eval logins: " + ("; ".join(problems) if problems else "present"))
    embed = embed_settings(os.environ)
    print("semantic search: " + (f"{embed['USER_MEMORY_EMBED_URL']} "
                                 f"{embed.get('USER_MEMORY_EMBED_MODEL', '(default model)')}"
                                 if embed else "off (keyword only)"))
    print("models: " + ", ".join(
        f"{c} {CLAUDE_MODEL if c == 'claude' else CODEX_MODEL + ' at ' + CODEX_EFFORT + ' effort'}"
        for c in clients))
    if "codex" in clients:
        # A superseded name is caught here, before a batch runs on it.
        stale = stale_models(codex_models(), [CODEX_MODEL, CODEX_DEFAULT_MODEL, CODEX_TOP_MODEL])
        if stale:
            sys.exit("refusing: " + "; ".join(stale))
    for b in binaries:
        print(f"binary {b['label']}: {b['path']} sha256 {b['sha256']}")
    same = same_builds(binaries)
    if same:
        sys.exit(f"refusing: {same}")
    if args.dry_run:
        print("guard watches:")
        for path, digest in fingerprint().items():
            print(f"  {digest[:16]}  {path}")
        print(f"  plus: no snapshot under {REAL_SESSIONS} named after a trial session")
        print(f"  plus: no new folder under {REAL_CLAUDE_PROJECTS} named after a trial path")
        return
    keep = kept_batches(os.environ)
    batch = OUT / time.strftime("%Y%m%d-%H%M%S")
    batch.mkdir(parents=True)
    trials = start_trials(batch.name, keep)
    with open(batch / "guard.log", "w") as log:
        guard = Guard(log)
        guard.check("baseline re-read")
        (batch / "binaries.json").write_text(json.dumps(binaries, indent=1))
        if args.rehearse:
            failures = client_probes(clients, trials, Path(binaries[-1]["path"]))
            guard.check("client probes")
            failures += sum(rehearse(scenarios, clients, trials, Path(b["path"]),
                                     spec.get("projects", {}), guard) for b in binaries)
            print(f"\nrehearsal: {failures} check(s) failed; report: {batch}")
            sys.exit(1 if failures else 0)
        if problems:
            sys.exit("refusing: " + "; ".join(problems))
        if counted_runs() + planned > args.budget:
            sys.exit("refusing: the batch would exceed the run budget")
        results = []
        # Builds take turns run by run, so a change in the model or the load
        # during a batch falls on each of them alike.
        for scenario, (client, repeat), b in batch_order(scenarios, clients, binaries, args.repeat):
            result = trial(scenario, client, trials, Path(b["path"]), spec.get("projects", {}), guard)
            result["repeat"] = repeat
            result["binary"] = b["label"]
            name = f"{scenario['name']}-{client}-{repeat}"
            if len(binaries) > 1:
                name = f"{b['label']}-{name}"
            (batch / f"{name}.json").write_text(json.dumps(result, indent=1))
            results.append(result)
            outcome = ("ERROR" if result["errors"] else "VOID" if result.get("void")
                       else "PASS" if result["passed"] else "FAIL")
            print(f"{outcome} {name}", flush=True)
            for error in result["errors"]:
                print(f"    {error}", flush=True)
            if result.get("void"):
                print(f"    void: {result['void']}", flush=True)
            for r in result["checks"]:
                if not r["passed"]:
                    tag = " (informational)" if r["informational"] else ""
                    print(f"    {r['check']}{tag}: {r['detail']}", flush=True)
    print(summarize(batch, results))
    print(f"\nreport: {batch}")


def summarize(batch, results):
    """Writes and returns the pass-rate table of a batch, with the binaries
    it ran."""
    summary = {}
    for r in results:
        key = (r["scenario"], r["client"], r.get("binary", "-"))
        passed, total, errored, voided, real, sent = summary.get(key, (0, 0, 0, 0, 0, 0))
        sent_origins = [o for s in r["steps"] for o in s["origins"]]
        void = bool(r.get("void")) and not r.get("errors")
        summary[key] = (passed + (r["passed"] and not void), total + 1, errored + bool(r.get("errors")),
                        voided + void, real + sum(o["real"] for o in sent_origins),
                        sent + len(sent_origins))
    lines = [f"# Eval batch {batch.name}", ""]
    saved = batch / "binaries.json"
    if saved.exists():
        lines += ["Binaries under test:", ""]
        lines += [f"- {b['label']}: `{b['path']}` sha256 `{b['sha256']}`"
                  for b in json.loads(saved.read_text())]
        lines.append("")
    lines += ["| scenario | client | binary | passed | origins with the real conversation id |",
              "|---|---|---|---|---|"]
    # Errored and void runs count in the total, not as scenario failures.
    lines += [f"| {s} | {c} | {b} | {p}/{t}{f' ({e} errored)' if e else ''}{f' ({v} void)' if v else ''} "
              f"| {f'{o}/{n}' if n else '-'} |"
              for (s, c, b), (p, t, e, v, o, n) in sorted(summary.items())]
    lines += ["", usage_table(results), "", f"Agent sessions counted so far: {counted_runs()}"]
    text = "\n".join(lines) + "\n"
    (batch / "summary.md").write_text(text)
    return text


def usage_table(results):
    """Tokens and estimated cost of a batch per scenario, client, binary and
    model, with a total row. A trial saved before usage was recorded adds nothing."""
    rows, total = {}, {}
    for r in results:
        for model, used in (r.get("usage") or {}).items():
            add_usage(rows.setdefault((r["scenario"], r["client"], r.get("binary", "-"), model), {}),
                      {model: used})
            add_usage(total, {"all": used})

    def cells(used):
        cost = used["cost_usd"]
        return (f"{used['input']:,} | {used['cached_input']:,} | {used['output']:,} | "
                f"{used['reasoning']:,} | {'-' if cost is None else f'${cost:.2f}'}")
    lines = ["| scenario | client | binary | model | input | cached input | output | reasoning | cost |",
             "|---|---|---|---|---|---|---|---|---|"]
    lines += [f"| {s} | {c} | {b} | {m} | {cells(used[m])} |"
              for (s, c, b, m), used in sorted(rows.items())]
    if total:
        lines.append(f"| all | | | | {cells(total['all'])} |")
    return "\n".join(lines)


def under_test(specs):
    """The binaries a run tests, from `--binary [LABEL=]PATH` values: each
    with its label (the file's name unless given), resolved path and
    SHA-256, so the report says which build each result is from."""
    binaries = []
    for spec in specs:
        label, _, path = spec.rpartition("=") if "=" in spec else ("", "", spec)
        path = Path(path).expanduser().resolve()
        if not os.access(path, os.X_OK):
            sys.exit(f"build the binary first: {path}")
        binaries.append({"label": label or path.name, "path": str(path),
                         "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    if len({b["label"] for b in binaries}) < len(binaries):
        sys.exit("refusing: two binaries share a label; give each its own, as LABEL=PATH")
    return binaries


def same_builds(binaries):
    """Why a run comparing builds would compare one build with itself, or
    None: two binaries with the same contents."""
    seen = {}
    for b in binaries:
        if b["sha256"] in seen:
            other = seen[b["sha256"]]
            return (f"{other['label']} ({other['path']}) and {b['label']} ({b['path']}) are the same "
                    "file contents, so the run would compare one build with itself. The likely cause: "
                    "a build folder that took the second commit as already built, because `git "
                    "archive` stamps files with the commit's time; extract with `tar -x -m` "
                    "(evals/README.md, Running a batch).")
        seen[b["sha256"]] = b
    return None


def resummarize(batch):
    """Recomputes a saved batch's summary from its trial files."""
    results = [json.loads(path.read_text()) for path in sorted(batch.glob("*-[0-9]*.json"))]
    return summarize(batch, results)


if __name__ == "__main__":
    main()
