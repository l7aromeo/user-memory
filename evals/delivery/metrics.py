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
overlaps a point the task needs of it. A wrong one is either about this
action (a point it overlaps is about what the action runs or edits, or
about the task's kind of work) and not needed in this task, or about
something else.

Rules that name no action ("start rule") are their own row: their
deliveries and needs stay out of the totals the precision budget reads.
"""

from collections import Counter, defaultdict

# Where a need's timing falls: before its action, at it, after it, never.
EARLY, ON_TIME, LATE, NEVER = "early", "on time", "late", "never"


def decisions(candidate, bars, action_kind):
    """Which ways `candidate` is delivered at bars `bars`: pushed, pushed
    after a failure, held before a command. With `bars["anchor"]`, a push
    also needs an anchor of the action that fired. `bars["scale"]` names
    the quantity the bars test (the matcher's `score` by default)."""
    score = candidate[bars.get("scale", "score")]
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


def subject_of(about):
    """The subject a memory point or action is about, as one key: a
    command, a component (its files and its design) or a kind of work."""
    if not about:
        return None
    if about[0] == "command":
        return tuple(about)
    if about[0] in ("file", "component"):
        return ("component", about[1])
    return tuple(about)


def about_action(candidate, memory, action, task_kind):
    """Whether a delivered passage is about the action it was delivered
    at: a point it overlaps (any, for a context passage) is about the same
    subject, or about the task's kind of work."""
    wanted = {subject_of(action.get("about")), ("work", task_kind)} - {None}
    for point in memory["points"]:
        if candidate["slot"] == 255 or overlaps(candidate, point):
            if subject_of(point.get("about")) in wanted:
                return True
    return False


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
    kind_of_work = task.get("kind")
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
            memory = memories[c["id"]]
            shape = memory["shape"]
            ok = right(c, needed_points)
            # The way it is delivered: a hold comes before the command, a
            # push after it; a failure push only when the plain bar misses.
            way = "hold" if "hold" in ways else ("push" if "push" in ways else "failure")
            held_here |= way == "hold"
            groups = ([f"shape:{shape}"] if shape == "start rule"
                      else ["all", f"way:{way}", f"signal:{signal}", f"shape:{shape}"])
            if signal == "prompt" and shape != "start rule":
                groups.append("prompt literal" if actions[i].get("literal") else "prompt plain")
            wrong = None if ok else ("about" if about_action(c, memory, actions[i], kind_of_work)
                                     else "elsewhere")
            for group in groups:
                counts[f"delivered|{group}"] += 1
                counts[f"right|{group}"] += ok
                if wrong:
                    counts[f"wrong {wrong}|{group}"] += 1
            if shape == "lesson":
                counts[f"lesson {lesson_moment(memory, i, actions, way)}|{'right' if ok else 'wrong'}"] += 1
            if shape == "merged" and ok:
                counts["merged delivered"] += 1
                counts["merged whole"] += any(holds_whole(c, p) for p in needed_points[c["id"]])
        # Long against short: a short memory needed here, and whether a
        # long merged memory's passage not needed here outranks it.
        needed_here = {n["memory"] for n in task["needs"] if n["action"] == i}
        for c in candidates:
            if c["id"] in needed_here and memories[c["id"]]["shape"] != "merged":
                counts["short needed"] += 1
                counts["long over short"] += any(
                    o["score"] > c["score"] and memories[o["id"]]["shape"] == "merged"
                    and not right(o, {o["id"]: [memories[o["id"]]["points"][n["point"]]
                                                for n in task["needs"]
                                                if n["memory"] == o["id"] and n["action"] == i]})
                    for o in candidates)
        if i == 0:
            counts["prompts"] += 1
            counts["prompts literal"] += bool(actions[0].get("literal"))
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
        groups = [f"subject:{need['subject']}", f"shape:{shape}"]
        if need["subject"] != "start rule":
            groups.append("all")
            # The matcher's own recall: needs whose point an anchor can match.
            if point.get("anchored"):
                groups.append("anchored")
        for group in groups:
            counts[f"needed|{group}"] += 1
            counts[f"found|{group}"] += when != NEVER
            counts[f"timing {when}|{group}"] += 1
            if when == NEVER:
                counts[f"missed {why_missed(need, point, matched, bars, actions)}|{group}"] += 1
        if need["subject"] == "failure":
            counts["failure needs"] += 1
            counts["failure found at its action"] += need["action"] in at
        if need["subject"] in ("command", "failure") and actions[need["action"]]["kind"] == "command":
            counts["command needs"] += 1
            counts["held before its action"] += any(
                first.get(key) == need["action"] and was_held(key, need["action"], matched, bars, actions)
                for key in first if key[0] == need["memory"])
    return counts


# Why a need was never delivered, from what its own action matched.
NO_ANCHOR = "the memory holds no anchor"
NOT_MATCHED = "anchored, its passage not among the candidates"
SPELLED = "anchored, the action spells it differently"
BELOW = "matched, below the bar"


def why_missed(need, point, matched, bars, actions):
    """Why a need was never delivered, judged at its action: its point
    names nothing an anchor can match; or it does and its passage was not
    a candidate, or was one without an anchor of the action, or passed
    every test but the score."""
    if not point.get("anchored"):
        return NO_ANCHOR
    here = [c for c in matched[need["action"]] if c["id"] == need["memory"]
            and (c["slot"] == 255 or overlaps(c, point))]
    if not here:
        return NOT_MATCHED
    if not any(c["anchored"] for c in here):
        return SPELLED
    return BELOW


# When a lesson was shown, against the runs of the command it is about.
BEFORE_FAILURE = "before its command, which fails later"
AT_FAILURE = "at its command's failure"
AFTER_FAILURE = "after its command failed"
BEFORE_SUCCESS = "before its command, which succeeds"
AFTER_SUCCESS = "after its command succeeded"
NOT_RUN = "in a task that never runs its command"


def lesson_moment(memory, i, actions, way):
    """When, against its command's runs in the task, a lesson delivered at
    action `i` was shown: a hold shows it before action `i` runs, a push
    after."""
    subject = subject_of(memory["points"][0].get("about"))
    runs = [j for j, a in enumerate(actions) if subject_of(a.get("about")) == subject]
    failures = [j for j in runs if actions[j].get("failed")]
    # Half an action earlier: a hold comes before its command runs.
    shown = i - 0.5 if way == "hold" else i
    if failures:
        return (BEFORE_FAILURE if shown < failures[0] else AT_FAILURE if shown == failures[0]
                else AFTER_FAILURE)
    if runs:
        return BEFORE_SUCCESS if shown < runs[0] else AFTER_SUCCESS
    return NOT_RUN


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
