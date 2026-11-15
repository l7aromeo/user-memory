"""The delivery benchmark's measures, from what the matcher matched at each
action of a task and what the task needed then.

A task is the generator's: its actions and its needs, each need a memory
and the point of it the work needs at one action. `matched[i]` is what the
matcher printed for action i: the candidates, best first, each with its
score and whether it holds a term of the action that fired (`fired`), of a
failed action that fired (`failed`) and an anchor of a command that fired
(`command`).

The bars decide what is delivered, as the matcher's `push`, `failure` and
`hold` do, so a sweep can try other bars on one run. A passage is shown
once per task, as #117 shows it once per context: a later match of the
same passage delivers nothing.

A delivery is right when its memory is needed in the task and its passage
overlaps a point the task needs of it.
"""

from collections import Counter, defaultdict

# Where a need's timing falls: before its action, at it, after it, never.
EARLY, ON_TIME, LATE, NEVER = "early", "on time", "late", "never"


def decisions(candidate, bars, action_kind):
    """Which ways `candidate` is delivered at bars `bars`: pushed, pushed
    after a failure, held before a command. With `bars["anchor"]`, a push
    also needs an anchor of the action that fired."""
    score = candidate["score"]
    out = set()
    # #125's change for pushes below their precision: a push needs an
    # anchor of the action, and stems only order what passed.
    anchored = candidate.get("anchored", False) or not bars.get("anchor")
    if action_kind == "prompt" and "prompt" in bars:
        # A prompt's own bar, on stems: it names no command or file.
        if candidate["fired"] and score >= bars["prompt"]:
            out.add("push")
    elif candidate["fired"] and anchored and score >= bars["push"]:
        out.add("push")
    if candidate["failed"] and score >= bars["failure"]:
        out.add("failure")
    if action_kind == "command" and candidate["command"] and score >= bars["hold"]:
        out.add("hold")
    return out


def overlaps(candidate, point):
    start = candidate["start"]
    end = start + candidate["length"]
    return start < point["end"] and point["start"] < end


def holds_whole(candidate, point):
    start = candidate["start"]
    return start <= point["start"] and point["end"] <= start + candidate["length"]


def right(candidate, needed_points):
    """Whether a delivered candidate holds a point its task needs: a
    context passage counts for any need of its memory."""
    points = needed_points.get(candidate["id"])
    if not points:
        return False
    if candidate["slot"] == 255:
        return True
    return any(overlaps(candidate, p) for p in points)


def evaluate(task, matched, memories, bars):
    """The counts one task gives: deliveries and the right ones by way,
    signal and shape; needs and the delivered ones by subject and shape,
    with their timing; holds on actions no memory names; and how often a
    long memory's delivered passage holds its needed point whole.

    `memories` maps a memory id to its `shape` and `points`."""
    actions = task["actions"]
    needed_points = defaultdict(list)
    for need in task["needs"]:
        needed_points[need["memory"]].append(memories[need["memory"]]["points"][need["point"]])
    counts = Counter()
    shown = set()
    first = {}  # (memory, slot) -> action delivered at
    for i, candidates in enumerate(matched):
        kind = actions[i]["kind"]
        signal = "prompt" if i == 0 else kind
        named = any(need["action"] == i for need in task["needs"])
        held_here = False
        for c in candidates:
            ways = decisions(c, bars, kind)
            if not ways:
                continue
            key = (c["id"], c["slot"])
            if key in shown:
                continue
            shown.add(key)
            first[key] = i
            shape = memories[c["id"]]["shape"]
            ok = right(c, needed_points)
            # The way it is delivered: a hold comes before the command, a
            # push after it; a failure push only when the plain bar misses.
            way = "hold" if "hold" in ways else ("push" if "push" in ways else "failure")
            held_here |= way == "hold"
            for group in ("all", f"way:{way}", f"signal:{signal}", f"shape:{shape}"):
                counts[f"delivered|{group}"] += 1
                counts[f"right|{group}"] += ok
            if shape == "merged" and ok:
                counts["merged delivered"] += 1
                counts["merged whole"] += any(holds_whole(c, p) for p in needed_points[c["id"]])
        if kind == "command" and not named:
            counts["unnamed commands"] += 1
            counts["unnamed held"] += held_here
    for need in task["needs"]:
        point = memories[need["memory"]]["points"][need["point"]]
        shape = memories[need["memory"]]["shape"]
        at = [first[key] for key in first if key[0] == need["memory"]
              and (key[1] == 255 or overlaps_slot(key, point, matched))]
        when = NEVER if not at else (EARLY if min(at) < need["action"]
                                      else ON_TIME if min(at) == need["action"] else LATE)
        for group in ("all", f"subject:{need['subject']}", f"shape:{shape}"):
            counts[f"needed|{group}"] += 1
            counts[f"found|{group}"] += when != NEVER
            counts[f"timing {when}|{group}"] += 1
        if need["subject"] == "failure":
            counts["failure needs"] += 1
            counts["failure found at its action"] += need["action"] in at
        if need["subject"] in ("command", "failure") and actions[need["action"]]["kind"] == "command":
            counts["command needs"] += 1
            counts["held before its action"] += any(
                first.get(key) == need["action"] and was_held(key, need["action"], matched, bars, actions)
                for key in first if key[0] == need["memory"])
    return counts


def overlaps_slot(key, point, matched):
    """Whether the passage `key` names, as some action matched it,
    overlaps `point`."""
    for candidates in matched:
        for c in candidates:
            if (c["id"], c["slot"]) == key:
                return overlaps(c, point)
    return False


def was_held(key, i, matched, bars, actions):
    for c in matched[i]:
        if (c["id"], c["slot"]) == key:
            return "hold" in decisions(c, bars, actions[i]["kind"])
    return False


def ratio(part, whole):
    return part / whole if whole else None
