"""The delivery benchmark's synthetic world: a store's memories and the
whole tasks run against it, every part drawn from one seed.

A world has a project with components, files and the commands its work
runs. Memories are written about those subjects: notes on a command, a
file or a component, lessons learned from a failure, the user's rules for
a kind of work, and long merged memories holding many such points. Tasks
are a prompt and a list of actions, in five kinds of work; for each action
the generator knows which memories, and which point of a long one, the
work needs then.

When a memory is needed, by shape:
- a note on a command, at every run of it: "`cargo test` needs the
  database running" holds whether the run then passes or fails;
- a lesson, at a failed run of its command only: "when `cargo publish`
  times out, wait and retry" changes nothing on a run that succeeds;
  shown earlier in a task where the run fails, it counts as early;
- a note on a file, at the edit of that file;
- a design decision on a component, at the prompt naming the component
  and at a task handed to a helper on it;
- a rule naming a command, at every run of it; a rule naming no action
  ("start rule"), at the prompt of each task of its kind of work;
- a point of a long merged memory, as the shape it would have alone;
- a look-alike or filler memory, never.

What keeps the benchmark honest:
- a needed memory names its subject the way a person writes it down, not
  the way the action says it: a command in a code span only some of the
  time, a component by one of its synonyms, its advice in its own words;
- every needed memory has look-alikes that share its common words and
  its program or folder but apply to another subcommand, file or
  component;
- the filler memories come from evals/scale's Zipf vocabulary of made-up
  words, with the world's common words mixed in;
- some tasks run commands no memory names, to count holds that should
  never happen.

Nothing here is a real memory.
"""

import importlib.util
import random
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

_spec = importlib.util.spec_from_file_location("scale_run", ROOT / "evals/scale/run.py")
scale = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scale)

# The five kinds of work #125 names.
KINDS = ["change", "release", "review", "research", "split"]

# The project the tasks run in, and one more whose memories are out of
# scope for them unless global.
PROJECT = scale.PROJECTS[0]
OTHER_PROJECT = scale.PROJECTS[1]

# A project's components, each with the words a person names it by. A
# memory and an action pick their word apart, as a paraphrase would.
COMPONENTS = {
    "parser": ["parser", "reader", "tokenizer"],
    "exporter": ["exporter", "export", "writer"],
    "scheduler": ["scheduler", "job queue", "cron runner"],
    "cache": ["cache", "memo layer", "lookup cache"],
    "auth": ["auth", "login", "sign-in"],
    "billing": ["billing", "invoices", "payments"],
    "renderer": ["renderer", "templates", "page builder"],
    "migrator": ["migrator", "schema migrations", "upgrade step"],
    "indexer": ["indexer", "search index", "word index"],
    "notifier": ["notifier", "alerts", "notifications"],
    "uploader": ["uploader", "file upload", "attachments"],
    "ledger": ["ledger", "accounts", "balances"],
}

# Commands, each with the words a person uses for what it does.
COMMANDS = {
    ("git", "commit"): ["committing", "recording changes"],
    ("git", "push"): ["pushing", "uploading the branch"],
    ("git", "tag"): ["tagging", "marking the release"],
    ("git", "rebase"): ["rebasing", "replaying commits"],
    ("cargo", "build"): ["building", "compiling"],
    ("cargo", "test"): ["testing", "the test suite"],
    ("cargo", "publish"): ["publishing", "uploading the crate"],
    ("just", "check"): ["the checks", "lint and test"],
    ("gh", "release"): ["the release page", "release notes"],
    ("gh", "pr"): ["the pull request", "review requests"],
    ("cargo", "clippy"): ["linting", "clippy"],
    ("git", "stash"): ["stashing", "setting changes aside"],
    ("npm", "install"): ["installing packages", "fetching dependencies"],
    ("npm", "publish"): ["publishing the package", "uploading the package"],
    ("docker", "build"): ["building the image", "container builds"],
    ("docker", "push"): ["pushing the image", "registry uploads"],
    ("kubectl", "apply"): ["deploying", "rolling out"],
    ("kubectl", "rollout"): ["rollouts", "restarting pods"],
    ("terraform", "apply"): ["provisioning", "infrastructure changes"],
    ("terraform", "plan"): ["planning", "previewing changes"],
    ("make", "docs"): ["building the docs", "documentation builds"],
    ("pytest", ""): ["python tests", "the python suite"],
}

# Subcommands no task runs, which look-alikes are about: they share a
# needed note's program and none of its subject.
UNRUN = {
    "git": ["bisect", "log", "blame", "reflog", "cherry-pick"],
    "cargo": ["doc", "bench", "tree", "update", "fmt"],
    "just": ["setup", "docs", "clean"],
    "gh": ["issue", "run", "api"],
    "npm": ["audit", "ci", "outdated"],
    "docker": ["compose", "run", "logs"],
    "kubectl": ["logs", "describe", "exec"],
    "terraform": ["init", "fmt", "validate"],
    "make": ["clean", "lint"],
    "pytest": ["--collect-only", "--lf"],
}

# The commands each kind of work runs; every world has these.
WORK = {
    "change": [("cargo", "build"), ("cargo", "test"), ("git", "commit"), ("git", "push")],
    "release": [("just", "check"), ("cargo", "publish"), ("git", "tag"), ("gh", "release")],
    "review": [("gh", "pr")],
    "research": [],
    "split": [("cargo", "test"), ("git", "rebase")],
}

# Flags commands carry, which never join the window.
FLAGS = ["--dry-run", "--all", "-v", "--locked", "--force-with-lease", "-s", "--watch",
         "--release", "--no-verify", "-n", "--quiet"]

# Folders and extensions a component's file lives under.
FOLDERS = ["src", "src/core", "lib", "app", "docs", "scripts", "tests", "config"]
EXTENSIONS = ["rs", "py", "ts", "go", "md", "yml", "toml", "sh"]
# Other files in a component's folder, which look-alikes are about.
NEIGHBOURS = ["helpers", "util", "types", "errors", "consts"]

# Common words of the work, which many memories share and so weigh little.
COMMON = ["build", "test", "release", "change", "code", "file", "check", "run", "update",
          "version", "docs", "branch", "commit", "fix", "error", "config", "review",
          "project", "team", "step", "before", "after", "always", "never", "first"]

# Advice a memory gives about a subject, in different words each time.
ADVICE = [
    "{subject} needs {thing} first, or it {failure}.",
    "Before {subject}, {do}; otherwise it {failure}.",
    "{subject}: {do}, because it {failure} when you don't.",
    "Remember that {subject} {failure} unless you {do}.",
    "For {subject} we agreed to {do}; skipping it {failure}.",
]
THINGS = ["a clean tree", "the signing key loaded", "the lock file current", "a fresh token",
          "the staging config", "the feature flag off", "the cache cleared", "a quiet branch",
          "the changelog entry", "the right region set"]
DOS = ["load the signing key", "run it against staging", "pin the toolchain", "clear the cache",
       "write the changelog entry", "ask the owner", "bump the version by hand",
       "check the region", "wait for the nightly run", "set the timeout to ninety seconds",
       "keep the old binary for ninety days", "use the internal registry"]
FAILURES = ["burns the version number", "leaves a half-written state", "times out in CI",
            "pages whoever is on call", "corrupts the lock file", "breaks the nightly build",
            "uploads to the wrong registry", "drops the oldest records",
            "silently skips the migration", "locks other sessions out"]

# What a prompt or a report says went wrong: a person's own words for a
# symptom, never a memory's phrase for a cause.
SYMPTOMS = ["hangs on large inputs", "shows stale numbers", "crashes at startup",
            "returns empty pages", "logs the same line twice", "is slow after an hour",
            "rejects valid input", "loses the last edit", "prints garbled dates",
            "spins at full processor"]

# The user's rules for a kind of work, as a person says them. A rule that
# names an action is needed at that action, the one way this matcher
# delivers a rule (#117); a rule for the whole kind of work reaches the
# agent another way (the instruction files, session start, #117's rules at
# the start of the work) and isn't counted here. A rule names its command
# in a code span only some of the time.
RULES = {
    "change": [("Write each new module's name in the glossary file.", None),
               ("Run the lint pass twice before a commit made on a Friday.", ("git", "commit")),
               ("Spell out every abbreviation the first time a file uses it.", None)],
    "release": [("Tag a release only after the mascot file is updated.", ("git", "tag")),
                ("Publish only once the build server shows a green lantern.", ("cargo", "publish")),
                ("Every release gets a codename from the list of rivers.", ("gh", "release"))],
    "review": [("Answer a pull request about the parser with a comment in rhyme.", ("gh", "pr")),
               ("Mark each review note with the module's colour code.", None),
               ("Approve a change only after a second look the next morning.", None)],
    "research": [("Keep every source in the shared reading list with a one-word mood.", None),
                 ("Write findings on the blue template, never the green one.", None),
                 ("End each report with three questions for the next reader.", None)],
    "split": [("Only the helper named Heron may push to the staging branch.", ("git", "push")),
              ("Give each helper a bird's name and a folder of its own.", None),
              ("Rebase onto the rehearsal branch before any demo.", ("git", "rebase"))],
}

# How often a command or file note names its subject as written (a code
# span, a path), so an anchor can match; the rest say it in words only.
ANCHORED_SHARE = 0.6
# How often a memory names a subject with the action's own word; otherwise
# it uses a synonym.
SAME_WORDS_SHARE = 0.5
# Share of command notes with a lesson about the command failing.
LESSON_SHARE = 0.5
# Share of notes written with a context saying when they apply.
CONTEXT_SHARE = 0.3
# Points of a long merged memory, each about one subject.
MERGED_POINTS = (6, 12)
# Common words padding a merged memory's point, so each is a passage.
POINT_PADDING = (14, 30)
# Share of prompts that name a literal, as real prompts often do: a file, a
# command or a pull request's number.
PROMPT_LITERAL_SHARE = 0.4
# Share of commands in a task that fail, and of tasks whose commands no
# memory names.
FAILURE_SHARE = 0.15
UNNAMED_TASK_SHARE = 0.15
# Tasks per kind of work, and the sessions a project's tasks spread over.
TASKS_PER_KIND = 8
SESSIONS = 10
# Probes of a need whose words fall in two passages of one long memory
# (#125): each a long memory with one word in each half of its point, a
# short memory holding each word alone, and a search naming both.
SPLIT_PROBES = 8
# Common words around each half's word, enough that each half is a passage.
SPLIT_HALF_WORDS = (45, 60)

# Share of a filler memory's words that are the world's common words.
FILLER_COMMON_SHARE = 0.15

# What a world of each store size holds: components, commands beyond the
# work's own, rules per kind of work, look-alikes per needed memory, and
# long merged memories. A 50-memory store holds a small project; larger
# ones hold the whole world and filler.
SHAPE = {
    50: {"components": 3, "extra_commands": 0, "rules": 1, "look_alikes": (1, 1), "merged": 1,
         "other_project": False},
    1_000: {"components": 12, "extra_commands": 12, "rules": 3, "look_alikes": (1, 3),
            "merged": 4, "other_project": True},
}


def shape(size):
    return SHAPE[50] if size < 1_000 else SHAPE[1_000]


class World:
    """One seeded world: memories and tasks for a store of `size`."""

    def __init__(self, seed, size):
        self.rng = random.Random(seed)
        self.size = size
        self.shape = shape(size)
        self.memories = []
        self.tasks = []
        self.needed = {}  # subject -> [(memory id, point index)]
        rng = self.rng
        self.components = rng.sample(sorted(COMPONENTS), self.shape["components"])
        work = sorted({c for commands in WORK.values() for c in commands})
        extra = [c for c in sorted(COMMANDS) if c not in work]
        self.commands = work + rng.sample(extra, self.shape["extra_commands"])
        self.files = {c: f"{rng.choice(FOLDERS)}/{c}.{rng.choice(EXTENSIONS)}" for c in COMPONENTS}
        projects = [PROJECT] + ([OTHER_PROJECT] if self.shape["other_project"] else [])
        for project in projects:
            self._notes(project, needed=project == PROJECT)
        self._rules()
        self._merged()
        self._look_alikes()
        self._tasks()
        self._split_probes()
        self._filler()
        rng.shuffle(self.memories)

    # --- memories -------------------------------------------------------

    def _word(self, synonyms):
        """A memory's word for a subject: the action's own only some of
        the time."""
        if self.rng.random() < SAME_WORDS_SHARE or len(synonyms) == 1:
            return synonyms[0]
        return self.rng.choice(synonyms[1:])

    def _advice(self, subject):
        rng = self.rng
        text = rng.choice(ADVICE).format(subject=subject, thing=rng.choice(THINGS),
                                         do=rng.choice(DOS), failure=rng.choice(FAILURES))
        return text[0].upper() + text[1:] if not text.startswith("`") else text

    def _memory(self, kind, memory_type, category, title, content, project, context="",
                points=None, about=None, anchored=False):
        """A memory. Each point says what it is `about` (a subject such as
        ["command", "git", "tag"], or None) and whether it names that as
        written, so an anchor can match (`anchored`)."""
        memory = {
            "id": str(uuid.UUID(int=self.rng.getrandbits(128), version=4)),
            "shape": kind, "type": memory_type, "category": category,
            "title": title[:scale.TITLE_CHARS].rstrip(), "content": content,
            "context": context, "scope": "project" if project else "global",
            "project": project,
            "points": points or [{"start": 0, "end": len(content), "about": about,
                                  "anchored": anchored}],
        }
        self.memories.append(memory)
        return memory

    def _need(self, subject, memory, point=0):
        self.needed.setdefault(subject, []).append((memory["id"], point))

    def _command_name(self, program, sub, anchored):
        if anchored:
            flags = f" {self.rng.choice(FLAGS)}" if self.rng.random() < 0.3 else ""
            return f"`{program} {sub}{flags}`".replace(" `", "`").replace("  ", " ")
        return self._word(COMMANDS[(program, sub)])

    def _context(self, words):
        return f"When {words}" if self.rng.random() < CONTEXT_SHARE else ""

    def _notes(self, project, needed):
        """A note on each command, file and component, and lessons from
        failures; the task project's are needed by its tasks."""
        rng = self.rng
        for program, sub in self.commands:
            anchored = rng.random() < ANCHORED_SHARE
            text = self._advice(self._command_name(program, sub, anchored))
            title = f"{program} {sub}".strip() + ": " + text.strip("`").split(";")[0][:60]
            m = self._memory("note", rng.choice(["fact", "decision", "constraint"]), "tooling",
                             title, text, project, self._context(f"running {program}"),
                             about=["command", program, sub], anchored=anchored)
            if needed:
                self._need(("command", program, sub), m)
            if rng.random() < LESSON_SHARE:
                lesson_anchored = rng.random() < ANCHORED_SHARE
                said = self._command_name(program, sub, lesson_anchored)
                lesson = f"When {said} fails with a timeout, {rng.choice(DOS)} and run it again."
                m = self._memory("lesson", "fact", "tooling",
                                 f"{program} {sub} failing: what helps".replace("  ", " "),
                                 lesson, project, about=["command", program, sub],
                                 anchored=lesson_anchored)
                if needed:
                    self._need(("failure", program, sub), m)
        for component in self.components:
            synonyms = COMPONENTS[component]
            path = self.files[component]
            anchored = rng.random() < ANCHORED_SHARE
            said = f"`{path}`" if anchored else f"the {self._word(synonyms)} code"
            text = self._advice(said)
            m = self._memory("note", rng.choice(["fact", "decision"]), "programming",
                             f"{self._word(synonyms).capitalize()}: {text.strip('`')[:60]}", text,
                             project, self._context(f"editing the {synonyms[0]}"),
                             about=["file", component], anchored=anchored)
            if needed:
                self._need(("file", component), m)
            decision = (f"The {self._word(synonyms)} keeps {rng.choice(THINGS)}; "
                        f"we chose that because it {rng.choice(FAILURES)} otherwise.")
            m = self._memory("decision", "decision", "design",
                             f"{self._word(synonyms).capitalize()} design decision", decision,
                             project, about=["component", component])
            if needed:
                self._need(("component", component), m)

    def _rules(self):
        rng = self.rng
        for kind, rules in RULES.items():
            for rule, command in rules[:self.shape["rules"]]:
                text = rule
                anchored = bool(command) and rng.random() < ANCHORED_SHARE
                if anchored:
                    text = f"{rule} (`{command[0]} {command[1]}`)"
                about = ["command", *command] if command else ["work", kind]
                m = self._memory("rule" if command else "start rule", "preference", "workflow",
                                 rule, text, "", about=about, anchored=anchored)
                self._need(("rule", *command) if command else ("start rule", kind), m)

    def _merged(self):
        """Long merged memories: a handbook of many points, each about one
        command or component; a need is one point."""
        rng = self.rng
        subjects = ([("command", p, s) for p, s in self.commands]
                    + [("component", c) for c in self.components])
        for _ in range(self.shape["merged"]):
            chosen = rng.sample(subjects, min(len(subjects), rng.randint(*MERGED_POINTS)))
            content, points = "", []
            for subject in chosen:
                anchored = subject[0] == "command" and rng.random() < ANCHORED_SHARE
                if subject[0] == "command":
                    text = self._advice(self._command_name(subject[1], subject[2], anchored))
                else:
                    text = self._advice(f"the {self._word(COMPONENTS[subject[1]])}")
                padding = " ".join(rng.choice(COMMON) for _ in range(rng.randint(*POINT_PADDING)))
                text = f"{text} Also {padding}."
                start = len(content) + 2 if content else 2
                content = f"{content} - {text}" if content else f"- {text}"
                points.append({"start": start, "end": len(content), "subject": list(subject),
                               "about": list(subject), "anchored": anchored})
            m = self._memory("merged", "fact", "workflow", f"{PROJECT.rsplit('/', 1)[-1]} handbook",
                             content, PROJECT, points=points)
            for i, point in enumerate(points):
                self._need(tuple(point["subject"]), m, i)

    def _look_alikes(self):
        """For each needed memory, ones that share its program, folder or
        common words and are about something else."""
        rng = self.rng
        for subject, needed in sorted(self.needed.items()):
            if subject[0] in ("rule", "start rule"):
                continue
            for _ in range(len(needed) * rng.randint(*self.shape["look_alikes"])):
                if subject[0] in ("command", "failure"):
                    program = subject[1]
                    other = rng.choice(UNRUN[program])
                    text = (f"`{program} {other}` {rng.choice(['needs', 'skips', 'reads'])} "
                            f"{rng.choice(THINGS)} on the {rng.choice(COMMON)} {rng.choice(COMMON)}.")
                    title = f"{program} {other}: {rng.choice(COMMON)} {rng.choice(COMMON)}"
                    self._memory("look-alike", "fact", "tooling", title, text, PROJECT,
                                 about=["command", program, other], anchored=True)
                else:
                    folder = self.files[subject[1]].rsplit("/", 1)[0]
                    path = f"{folder}/{rng.choice(NEIGHBOURS)}.{rng.choice(EXTENSIONS)}"
                    text = (f"`{path}` {rng.choice(['holds', 'wraps'])} the {rng.choice(COMMON)} "
                            f"{rng.choice(COMMON)} and needs {rng.choice(THINGS)} before a "
                            f"{rng.choice(COMMON)}.")
                    self._memory("look-alike", "fact", "programming",
                                 f"{path}: {rng.choice(COMMON)} {rng.choice(COMMON)}", text, PROJECT,
                                 about=["path", path], anchored=True)

    def _filler(self):
        rng = self.rng
        words = scale.vocabulary(rng)
        weights = scale.zipf_weights()

        def phrase(bounds):
            return " ".join(rng.choice(COMMON) if rng.random() < FILLER_COMMON_SHARE
                            else rng.choices(words, weights)[0]
                            for _ in range(rng.randint(*bounds)))

        while len(self.memories) < self.size:
            project = rng.choice(scale.PROJECTS) if rng.random() < scale.PROJECT_SHARE else ""
            self._memory("filler", rng.choice(scale.TYPES[:5]), rng.choice(scale.CATEGORIES),
                         phrase(scale.TITLE_WORDS).capitalize(), phrase(scale.CONTENT_WORDS),
                         project)

    def _split_probes(self):
        rng = self.rng
        words = sorted({w for w in scale.vocabulary(random.Random(rng.random())) if len(w) > 5})
        for p in range(SPLIT_PROBES):
            first, second = rng.sample(words, 2)
            halves = []
            for word in (first, second):
                padding = [rng.choice(COMMON) for _ in range(rng.randint(*SPLIT_HALF_WORDS))]
                padding.insert(rng.randrange(len(padding)), word)
                halves.append(" ".join(padding).capitalize() + ".")
            content = " ".join(halves)
            m = self._memory("split", "fact", "workflow", f"Notes on the {rng.choice(COMMON)} {rng.choice(COMMON)}",
                             content, PROJECT)
            for word in (first, second):
                self._memory("look-alike", "fact", "workflow",
                             f"Notes on the {rng.choice(COMMON)} {rng.choice(COMMON)}",
                             f"The {word} {rng.choice(COMMON)} {rng.choice(['holds', 'needs'])} {rng.choice(THINGS)}.",
                             PROJECT)
            self.tasks.append({"id": f"probe-{p}", "kind": "research", "project": PROJECT,
                               "session": p % SESSIONS, "unnamed": False, "probe": True,
                               "actions": [{"kind": "search", "text": f"{first} {second}", "failed": False}],
                               "needs": [{"action": 0, "memory": m["id"], "point": 0,
                                          "subject": "split"}]})

    # --- tasks ----------------------------------------------------------

    def _needs_at(self, needs, action, subject):
        for memory, point in self.needed.get(subject, []):
            needs.append({"action": action, "memory": memory, "point": point,
                          "subject": subject[0]})

    def _command(self, actions, needs, program, sub, failed):
        flags = " ".join(self.rng.sample(FLAGS, self.rng.randint(0, 2)))
        actions.append({"kind": "command", "failed": failed, "about": ["command", program, sub],
                        "text": " ".join(part for part in [program, sub, flags] if part)})
        self._needs_at(needs, len(actions) - 1, ("command", program, sub))
        self._needs_at(needs, len(actions) - 1, ("rule", program, sub))
        if failed:
            self._needs_at(needs, len(actions) - 1, ("failure", program, sub))

    def _file(self, actions, needs, component):
        actions.append({"kind": "file", "text": self.files[component], "failed": False,
                        "about": ["file", component]})
        self._needs_at(needs, len(actions) - 1, ("file", component))

    def _tasks(self):
        rng = self.rng
        n = 0
        for kind in KINDS:
            for t in range(TASKS_PER_KIND):
                actions, needs = [], []
                component = rng.choice(self.components)
                name = rng.choice(COMPONENTS[component])
                unnamed = rng.random() < UNNAMED_TASK_SHARE
                fail = lambda: rng.random() < FAILURE_SHARE
                prompt = {
                    "change": f"Fix the bug in the {name} where it {rng.choice(SYMPTOMS)}",
                    "release": f"Cut release 2.{t}.0 of {PROJECT.rsplit('/', 1)[-1]} and publish it",
                    "review": f"Review the pull request that reworks the {name}",
                    "research": f"Find out why the {name} {rng.choice(SYMPTOMS)} and report",
                    "split": f"Split the {name} rewrite between two helpers and merge their work",
                }[kind]
                literal = rng.random() < PROMPT_LITERAL_SHARE
                if literal:
                    prompt += {
                        "change": f" in `{self.files[component]}`",
                        "release": " with `cargo publish`",
                        "review": f" (pull request #{rng.randint(100, 999)})",
                        "research": f"; it lives in {self.files[component]}",
                        "split": f" ({self.files[component]})",
                    }[kind]
                actions.append({"kind": "prompt", "text": prompt, "failed": False,
                                "about": ["component", component], "literal": literal})
                self._needs_at(needs, 0, ("start rule", kind))
                if kind != "release":
                    self._needs_at(needs, 0, ("component", component))
                if unnamed:
                    # Commands no memory names: any hold on them is wrong.
                    for line in ["ls -la", "cat notes.txt", "echo done", "wc -l data.csv"]:
                        actions.append({"kind": "command", "text": line, "failed": False})
                elif kind == "change":
                    self._file(actions, needs, component)
                    for program, sub in WORK["change"]:
                        self._command(actions, needs, program, sub, fail())
                elif kind == "release":
                    actions.append({"kind": "file", "text": "CHANGELOG.md", "failed": False})
                    for program, sub in WORK["release"]:
                        self._command(actions, needs, program, sub, fail())
                elif kind == "review":
                    self._command(actions, needs, "gh", "pr", False)
                    self._file(actions, needs, component)
                    actions.append({"kind": "message", "failed": False,
                                    "text": f"The {name} change looks right; one question on the error path."})
                elif kind == "research":
                    actions.append({"kind": "search", "text": f"{name} {rng.choice(COMMON)}", "failed": False})
                    self._file(actions, needs, component)
                    program, sub = rng.choice(self.commands)
                    self._command(actions, needs, program, sub, fail())
                    actions.append({"kind": "message", "failed": False,
                                    "text": f"Report on the {name}: it {rng.choice(SYMPTOMS)}."})
                else:
                    actions.append({"kind": "plan", "failed": False,
                                    "text": f"Rewrite the {name}\nTest it\nMerge both parts"})
                    actions.append({"kind": "task", "failed": False,
                                    "text": f"Rewrite the {name} in {self.files[component]} and run the tests"})
                    self._needs_at(needs, len(actions) - 1, ("component", component))
                    self._file(actions, needs, component)
                    for program, sub in WORK["split"]:
                        self._command(actions, needs, program, sub, fail())
                self.tasks.append({"id": f"{kind}-{t}", "kind": kind, "project": PROJECT,
                                   "session": n % SESSIONS, "unnamed": unnamed,
                                   "actions": actions, "needs": needs})
                n += 1


def generate(seed, size):
    """The memories and tasks of the world drawn from `seed`, for a store
    of `size` memories, or a few more when its structured part alone is
    larger."""
    world = World(seed, size)
    return {"memories": world.memories, "tasks": world.tasks}
