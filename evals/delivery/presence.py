"""Which needed memories session start already lists: today's, from the
real binary, and a model of #120's and #123's session start, which orders
the index by use and prints a project's most-used knowledge.

The matcher misses what shares no anchor with the work, such as a design
decision named by a synonym. Such a memory can still be present when the
work starts, listed by session start. This measures how often.

Each task is one session, in an order drawn from the seed; the tasks run
`ROUNDS` times, each round in a new order, as a project's work recurs, so
a window longer than one round can be told apart. In the first session
there is no use history, so session start is today's: the real
`user-memory session-start` output, run in a throwaway home on a folder
whose remote is the world's project. In later sessions the model orders
the index by how many of the last `USE_WINDOW` sessions used each memory,
and prints the most-used part above it: a memory there is printed, its
text present, where the index lists only its line.

What a session used is counted two ways:
- delivered: what the matcher pushed or held in it;
- delivered or read: also every memory its task needed, as if the agent
  searched for and read each one it was not given.
The truth lies between: an agent reads some of what it lacks, not all.
"""

import random
import re
import subprocess
from collections import Counter, deque
from pathlib import Path

import metrics
import world

# The constants of #120's and #123's session start that this model sets;
# each says which issue owns it and what was measured. Measured: #123's
# use window, the share to be printed and how many are printed, and #120's
# ceiling for the most-used part. Not measured here: #120's floor for that
# part, the project rules' floor and ceiling, the working set and the list
# of new rules, which these worlds do not model.

# Session start's whole room as src/session.rs sets it today:
# `CLAUDE_LIMIT` less `ADDED_ROOM`.
CHARS = 9500
# The index's share of it: `CHARS` less `INDEX_MARGIN` in src/session.rs.
INDEX_CHARS = 9000
# #123: how many of a project's last sessions use is counted over. With
# reads counted at 10,000 memories, windows of 20, 40 and 80 list 0.93,
# 0.95 and 0.95 of later sessions' needs; 40 covers one round of these
# worlds' tasks, which recur unchanged. How fast real work drifts, which
# would make a long window stale, is not in the model.
USE_WINDOW = 40
# #123: the share of those sessions a memory must be used in to be printed
# as most-used knowledge. The worlds' essentials are needed by 0.7 of
# tasks: at window 40 and 10,000 memories, 0.25 and 0.5 print all of them
# and 0.75 prints 0.54. 0.25 also prints memories used less often, which
# leaves less room for the index (0.91 of needs listed against 0.95).
ESSENTIAL_SHARE = 0.5
# #123: at most this many most-used memories are printed. At 10,000
# memories 2, 5 and 15 print 0.44, 0.92 and 1.00 of the essentials and
# list 0.96, 0.96 and 0.95 of needs: printing fewer leaves the index more
# room. 15 prints every essential for 0.01 of needs listed.
ESSENTIALS_MAX = 15
# #123: a most-used memory this short is printed whole; a longer one as its
# line and one passage. Not separable in this model: 150, 300 and 600 give
# the same figures to two places.
ESSENTIAL_WHOLE_CHARS = 300
# #120: the most-used part's floor and ceiling of the room. The ceiling
# at 1,500, 3,000 and 4,500 prints 0.92, 1.00 and 1.00 of the essentials
# and lists 0.96, 0.95 and 0.95 of needs, at 10,000 memories; 3,000 is the
# least that prints them all. The floor is not varied yet; it is set to
# the smallest ceiling tried.
ESSENTIALS_FLOOR = 1500
ESSENTIALS_CHARS = 3000
# A passage printed for a long most-used memory, as delivery cuts them
# (src/memory/index/passages.rs `PASSAGE_CHARS`).
PASSAGE_CHARS = 400

# The values each constant is tried at.
USE_WINDOWS = [5, 10, 20, 40, 80]
ESSENTIAL_SHARES = [0.25, 0.5, 0.75]
# The most-used part's limits, each tried alone with the others at their
# constants, with reads counted, at `USE_WINDOW` and `ESSENTIAL_SHARE`.
LIMITS = {"most": [2, 5, 15], "whole": [150, 300, 600], "room": [1500, 3000, 4500]}

# Times the tasks run, each round in a new order: 40 to 48 tasks a round,
# so three rounds let a window of 80 sessions fill.
ROUNDS = 3

# A listed memory's line: its 8-character id after the indent.
LISTED = re.compile(r"^\s+([0-9a-f]{8}) ")


def project_folder(folder):
    """A folder whose git remote is the world's project, for the hooks."""
    repo = folder / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    subprocess.run(["git", "-C", str(repo), "remote", "add", "origin", f"https://{world.PROJECT}.git"],
                   check=True)
    return repo


def listed(text):
    return [m.group(1) for m in map(LISTED.match, text.splitlines()) if m]


def today(binary, env, folder):
    """Today's session start on the project, and the full index's lines in
    order with each line's length: the order ties keep."""
    repo = project_folder(folder)
    start = subprocess.run([str(binary), "session-start", str(repo)], env=env, check=True,
                           capture_output=True, text=True).stdout
    lines, cursor = [], None
    while True:
        args = [str(binary), "index", "--project", world.PROJECT, "--page-size", "200"]
        if cursor:
            args += ["--cursor", cursor]
        out = subprocess.run(args, env=env, check=True, capture_output=True, text=True).stdout
        lines += [(m.group(1), len(line)) for line, m in
                  ((line, LISTED.match(line)) for line in out.splitlines()) if m]
        cursor = next((line.split(": ", 1)[1].split(" ", 1)[0] for line in out.splitlines()
                       if line.startswith("next_cursor: ")), None)
        if not cursor:
            break
    header = sum(len(line) + 1 for line in out.splitlines()[:6])
    return set(listed(start)), lines, header


def used(task, matched, bars, reads):
    """The memories a session running `task` used: what was delivered, and
    with `reads`, everything its task needed."""
    out = Counter()
    for i, candidates in enumerate(matched):
        for c in candidates:
            if metrics.decisions(c, bars, task["actions"][i]["kind"]):
                out[c["id"][:8]] = max(out[c["id"][:8]], 1)
    if reads:
        for need in task["needs"]:
            # A read counts before a delivery (#123).
            out[need["memory"][:8]] = 2
    return out


def start(history, lines, header, memories, window, share, limits=None):
    """The ids the modelled session start lists or prints, and the ids it
    prints, from the use of the sessions in `history`, newest last."""
    recent = list(history)[-window:]
    weight = Counter()
    sessions = Counter()
    for session in recent:
        for memory, w in session.items():
            weight[memory] += w
            sessions[memory] += 1
    limits = {"most": ESSENTIALS_MAX, "whole": ESSENTIAL_WHOLE_CHARS, "room": ESSENTIALS_CHARS,
              **(limits or {})}
    shown = set()
    room = limits["room"]
    essentials = sorted((m for m in sessions if recent and sessions[m] / len(recent) >= share
                         and memories[m]["shape"] not in ("rule", "start rule")),
                        key=lambda m: -sessions[m])[:limits["most"]]
    used_room = 0
    for m in essentials:
        size = len(memories[m]["content"])
        size = size if size <= limits["whole"] else len(memories[m]["title"]) + PASSAGE_CHARS
        if used_room + size > room:
            break
        used_room += size
        shown.add(m)
    printed = set(shown)
    room = INDEX_CHARS - header - max(used_room, ESSENTIALS_FLOOR if essentials else 0)
    order = {m: i for i, (m, _) in enumerate(lines)}
    for m, length in sorted(lines, key=lambda line: (-weight[line[0]], order[line[0]])):
        if length + 1 > room:
            break
        room -= length + 1
        shown.add(m)
    return shown, printed


def tally(counts, phase, task, shown, printed=frozenset()):
    for need in task["needs"]:
        for group in (f"{phase}|{need['subject']}", f"{phase}|all"):
            counts[f"needed {group}"] += 1
            counts[f"listed {group}"] += need["memory"][:8] in shown
            counts[f"printed {group}"] += need["memory"][:8] in printed


def presence(binary, env, folder, drawn, matched, bars, seed):
    """Of the needs of each subject, the share whose memory session start
    lists: today's, as if every task began a project's first session; and
    the model's, for each way of counting use and each setting, in every
    session after the first."""
    first, lines, header = today(binary, env, folder)
    memories = {m["id"][:8]: m for m in drawn["memories"]}
    tasks = [t for t in drawn["tasks"] if not t.get("probe")]
    order = random.Random(seed)
    today_counts = Counter()
    for task in tasks:
        tally(today_counts, "first", task, first)
    tasks = [t for _ in range(ROUNDS) for t in order.sample(tasks, len(tasks))]
    # The model with no use at all: what its filling alone lists, so the
    # rows after it show what use adds.
    unused = start([], lines, header, memories, USE_WINDOW, ESSENTIAL_SHARE)
    rows = [(None, Counter())]
    for task in tasks[1:]:
        tally(rows[0][1], "later", task, *unused)
    for reads in (False, True):
        for window in USE_WINDOWS:
            for share in ESSENTIAL_SHARES:
                counts = Counter()
                history = deque(maxlen=max(USE_WINDOWS))
                for k, task in enumerate(tasks):
                    if k:
                        tally(counts, "later", task,
                              *start(history, lines, header, memories, window, share))
                    history.append(used(task, matched[task["id"]], bars, reads))
                rows.append(((reads, window, share), counts))
    for name, values in LIMITS.items():
        for value in values:
            counts = Counter()
            history = deque(maxlen=max(USE_WINDOWS))
            for k, task in enumerate(tasks):
                if k:
                    tally(counts, "later", task, *start(history, lines, header, memories, USE_WINDOW,
                                                        ESSENTIAL_SHARE, {name: value}))
                history.append(used(task, matched[task["id"]], bars, True))
            rows.append(((name, value), counts))
    return today_counts, rows
