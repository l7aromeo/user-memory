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
overlaps a point the task needs of it. A right one is needed now when a
need of that point is still ahead (a trap's need is before its command
runs, so only a hold or an earlier push meets it), and late otherwise. A
wrong one is either about this action (a point it overlaps is about what
the action runs or edits, or about the task's kind of work) and not needed
in this task, or about something else.

At most `PUSH_PER_EVENT` pushes are shown per action, in the store's
order; the rest wait for a later action that matches them (#117).

Rules that name no action ("start rule") and a project's essentials, which
most tasks need from their start ("essential"), are their own rows: their
deliveries and needs stay out of the totals the precision budget reads, as
session start serves them, not a match on the work.
"""

from collections import Counter, defaultdict

# Shapes whose deliveries and needs stay out of the totals.
OUTSIDE = ("start rule", "essential")

# Where a need's timing falls: before its action, at it, after it, never.
EARLY, ON_TIME, LATE, NEVER = "early", "on time", "late", "never"

# #117's limit on what one event pushes: more would bury the work.
PUSH_PER_EVENT = 3

# Bands of how many memories hold the rarest anchor of the action that a
# passage holds (the matcher's `fewest`).
BANDS = ["no anchor", "1", "2", "3-5", "6+"]


def band(fewest):
    return BANDS[0] if not fewest else "1" if fewest == 1 else "2" if fewest == 2 else (
        "3-5" if fewest <= 5 else "6+")


def shown_at(i, way):
    """When a delivery at action `i` is seen: a hold before the action
    runs, a push after it."""
    return i - 0.5 if way == "hold" else i


def in_time(shown, need):
    """Whether a passage seen at `shown` meets `need`: a trap's before its
    command runs, any other's by its action."""
    return shown < need["action"] if need["subject"] == "trap" else shown <= need["action"]


def decisions(candidate, bars, action_kind):
    """See `tested`; with `bars["scale"] == "kind"`, a command's
    passages are tested on `bars["command"]` and every other event's on
    `bars["other"]`; with `"either"`, a way passes when it passes on any
    of `bars["parts"]`."""
    if bars.get("scale") == "kind":
        return tested(candidate, {**bars, **bars["command" if action_kind == "command" else "other"]},
                      action_kind)
    if bars.get("scale") == "either":
        return set().union(*(tested(candidate, {**bars, **part}, action_kind) for part in bars["parts"]))
    return tested(candidate, bars, action_kind)


def tested(candidate, bars, action_kind):
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
    # With `bars["failure anchor"]`, a push after a failure needs an anchor
    # of the failed action too.
    if (candidate["failed"] and score >= bars["failure"]
            and (candidate.get("anchored") or not bars.get("failure anchor"))):
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
    subject, about what the action names (a prompt naming a command), or
    about the task's kind of work."""
    wanted = {subject_of(action.get("about")), subject_of(action.get("names")), ("work", task_kind)} - {None}
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


def evaluate(task, matched, memories, bars, earlier=None):
    """The counts one task gives: deliveries and the right ones by way,
    signal and shape, each needed now, late, about this action or about
    something else, and placed against its command's runs; needs and the
    delivered ones by subject, shape and holders' band, with their timing;
    holds and the actions they deny; holds on actions no memory names; and
    how often a long memory's delivered passage holds its needed point
    whole.

    `memories` maps a memory id to its `shape` and `points`. `earlier`
    holds the passages shown in the session's earlier tasks, and gains
    this task's: with `bars["remember"]` they are not shown again and
    meet a need as if seen before it; without, holding one again is
    counted. `bars["hold cap"]` limits the passages one hold shows, the
    rest pushed after the command (`bars["hold rest"] == "push"`) or
    dropped. With `bars["lesson oracle"]` a lesson is shown only after a
    failed run of its command: an upper bound, as the matcher cannot know
    a recovery lesson from a note."""
    actions = task["actions"]
    kind_of_work = task.get("kind")
    per_event = bars.get("per_event", PUSH_PER_EVENT)
    hold_cap = bars.get("hold cap")
    earlier = set() if earlier is None else earlier
    remember = bars.get("remember")
    needed_points = defaultdict(list)
    needs_of = defaultdict(list)
    for need in task["needs"]:
        point = memories[need["memory"]]["points"][need["point"]]
        needed_points[need["memory"]].append(point)
        needs_of[need["memory"]].append((point, need))
    counts = Counter({"tasks": 1})
    shown = set()
    first = {}  # (memory, slot) -> when it was seen
    for i, candidates in enumerate(matched):
        kind = actions[i]["kind"]
        signal = "prompt" if i == 0 else kind
        named = any(need["action"] == i for need in task["needs"])
        held_here = False
        held_now_here = False
        held_count = 0
        pushed_here = 0
        for c in candidates:
            ways = decisions(c, bars, kind)
            if not ways:
                continue
            key = (c["id"], c["slot"])
            if key in shown or (remember and key in earlier):
                continue
            # The way it is delivered: a hold comes before the command, a
            # push after it; a failure push only when the plain bar misses.
            way = "hold" if "hold" in ways else ("push" if "push" in ways else "failure")
            if bars.get("lesson oracle") and memories[c["id"]]["shape"] == "lesson" and not (
                    way != "hold" and actions[i].get("failed")
                    and subject_of(actions[i].get("about"))
                    == subject_of(memories[c["id"]]["points"][0].get("about"))):
                continue
            if way == "hold" and hold_cap is not None and held_count >= hold_cap:
                if bars.get("hold rest") != "push":
                    continue
                way = "push"
            if way == "hold":
                held_count += 1
                counts["held again"] += key in earlier
            else:
                if per_event is not None and pushed_here >= per_event:
                    continue
                pushed_here += 1
            shown.add(key)
            seen = shown_at(i, way)
            first[key] = seen
            memory = memories[c["id"]]
            shape = memory["shape"]
            ok = right(c, needed_points)
            held_here |= way == "hold"
            groups = ([f"shape:{shape}"] if shape in OUTSIDE
                      else ["all", f"way:{way}", f"signal:{signal}", f"shape:{shape}"])
            if signal == "prompt" and shape not in OUTSIDE:
                groups.append("prompt literal" if actions[i].get("literal") else "prompt plain")
            wrong = None if ok else ("about" if about_action(c, memory, actions[i], kind_of_work)
                                     else "elsewhere")
            now = ok and any(in_time(seen, need) for point, need in needs_of[c["id"]]
                             if c["slot"] == 255 or overlaps(c, point))
            moment = run_moment(memory, c, i, actions, way)
            how = "now" if now else "late" if ok else wrong
            held_now_here |= way == "hold" and now
            for group in groups:
                counts[f"delivered|{group}"] += 1
                counts[f"right|{group}"] += ok
                counts[f"{how}|{group}"] += 1
                if wrong:
                    counts[f"wrong {wrong}|{group}"] += 1
            counts[f"moment {moment}|shape:{shape}|{how}"] += 1
            if shape not in OUTSIDE:
                counts[f"band {band(c.get('fewest'))}|delivered"] += 1
                counts[f"band {band(c.get('fewest'))}|now"] += now
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
        if held_count:
            counts[f"hold size {held_count}"] += 1
        if actions[i].get("repeat"):
            # A second or later run of a busy command in this task.
            counts["repeat runs"] += 1
            counts["repeat held"] += held_count > 0
            counts["repeat pushed"] += held_count == 0 and pushed_here > 0
            counts["repeat nothing"] += held_count == 0 and pushed_here == 0
        if actions[i].get("busy"):
            # A run of a command with many notes: what it delivered, and
            # how much of that it needed now.
            counts["busy runs"] += 1
            counts["busy delivered"] += held_count + pushed_here
            counts["busy needed"] += sum(n["action"] == i for n in task["needs"])
        if kind == "command":
            counts["denials"] += held_here
            counts["denials with a need"] += held_now_here
            if actions[i].get("collides"):
                # Its argument names another command's subcommand.
                counts["colliding commands"] += 1
                counts["colliding held"] += held_here
                counts["colliding pushes"] += pushed_here
            elif not named:
                counts["unnamed commands"] += 1
                counts["unnamed held"] += held_here
    for need in task["needs"]:
        point = memories[need["memory"]]["points"][need["point"]]
        shape = memories[need["memory"]]["shape"]
        at = [first[key] for key in first if key[0] == need["memory"]
              and (key[1] == 255 or overlaps_slot(key, point, matched))]
        if remember:
            # Seen in an earlier task of the session: before this need.
            at += [-1 for key in earlier if key[0] == need["memory"]
                   and (key[1] == 255 or overlaps_slot(key, point, matched))]
        # Seen at its own action, held or pushed, is on time; a trap only
        # when held, as a push comes after the command ran.
        when = NEVER if not at else (EARLY if min(at) < need["action"] - 0.5
                                     else ON_TIME if in_time(min(at), need) else LATE)
        groups = [f"subject:{need['subject']}", f"shape:{shape}"]
        if need["subject"] not in OUTSIDE:
            groups.append("all")
            # The matcher's own recall: needs whose point an anchor can match.
            if point.get("anchored"):
                groups.append("anchored")
                groups.append(f"spelling {actions[need['action']].get('spelled') or 'as written'}")
                here = [c for c in matched[need["action"]] if c["id"] == need["memory"]
                        and (c["slot"] == 255 or overlaps(c, point))]
                groups.append(f"band {band(here[0].get('fewest')) if here else 'not a candidate'}")
        for group in groups:
            counts[f"needed|{group}"] += 1
            counts[f"found|{group}"] += when != NEVER
            counts[f"in time|{group}"] += when in (EARLY, ON_TIME)
            counts[f"timing {when}|{group}"] += 1
            if when == NEVER:
                counts[f"missed {why_missed(need, point, matched, bars, actions)}|{group}"] += 1
        if need["subject"] == "failure":
            counts["failure needs"] += 1
            counts["failure found at its action"] += need["action"] in at or need["action"] - 0.5 in at
        if need["subject"] in ("command", "failure", "trap") and actions[need["action"]]["kind"] == "command":
            counts["command needs"] += 1
            counts["held before its action"] += need["action"] - 0.5 in at
    earlier.update(shown)
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


# When a delivery was seen, against the runs of the command its point is
# about.
BEFORE_FAILURE = "before its command, whose next run fails"
BEFORE_SUCCESS = "before its command, whose next run succeeds"
AT_FAILURE = "at its command's failure"
AFTER_FAILURE = "after its command failed"
AFTER_SUCCESS = "at or after its command's success"
NOT_RUN = "about a command the task doesn't run"
NO_COMMAND = "about no command"


def run_moment(memory, candidate, i, actions, way):
    """When a passage delivered at action `i` was seen against the runs of
    the command its point is about: before the next run (by how that run
    ends), at a failed run, or after the last run before it."""
    points = [p for p in memory["points"] if candidate["slot"] == 255 or overlaps(candidate, p)]
    about = subject_of(points[0].get("about")) if points else None
    if not about or about[0] != "command":
        return NO_COMMAND
    runs = [j for j, a in enumerate(actions) if subject_of(a.get("about")) == about]
    if not runs:
        return NOT_RUN
    seen = shown_at(i, way)
    before = [j for j in runs if j <= seen]
    if not before:
        return BEFORE_FAILURE if actions[runs[0]].get("failed") else BEFORE_SUCCESS
    last = before[-1]
    if actions[last].get("failed"):
        return AT_FAILURE if last == seen else AFTER_FAILURE
    return AFTER_SUCCESS


def overlaps_slot(key, point, matched):
    """Whether the passage `key` names, as some action matched it,
    overlaps `point`."""
    for candidates in matched:
        for c in candidates:
            if (c["id"], c["slot"]) == key:
                return overlaps(c, point)
    return False


def ratio(part, whole):
    return part / whole if whole else None
