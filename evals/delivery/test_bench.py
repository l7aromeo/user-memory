"""Tests of the delivery benchmark's measures and generator, on cases
worked out by hand.

    python3 -I evals/delivery/test_bench.py
"""

import sys
import unittest
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import metrics  # noqa: E402
import world  # noqa: E402

BARS = {"push": 2.0, "failure": 1.0, "hold": 3.0}


def candidate(memory, score, slot=1, start=0, length=10, fired=True, failed=False,
              anchored=False, command=False, fewest=0):
    return {"id": memory, "slot": slot, "start": start, "length": length, "score": score,
            "fired": fired, "failed": failed, "anchored": anchored, "command": command,
            "fewest": fewest}


class Decisions(unittest.TestCase):
    def test_a_push_needs_the_bar_and_a_term_of_the_action(self):
        self.assertEqual(metrics.decisions(candidate("a", 2.0), BARS, "file"), {"push"})
        self.assertEqual(metrics.decisions(candidate("a", 1.99), BARS, "file"), set())
        self.assertEqual(metrics.decisions(candidate("a", 9, fired=False), BARS, "file"), set())

    def test_the_anchor_rule_lets_only_anchored_passages_push(self):
        bars = {**BARS, "anchor": True}
        self.assertEqual(metrics.decisions(candidate("a", 5), bars, "prompt"), set())
        self.assertEqual(metrics.decisions(candidate("a", 5, anchored=True), bars, "prompt"), {"push"})

    def test_a_failure_lowers_the_bar_and_a_hold_needs_a_command_anchor(self):
        self.assertEqual(metrics.decisions(candidate("a", 1.0, fired=False, failed=True), BARS, "command"),
                         {"failure"})
        self.assertEqual(metrics.decisions(candidate("a", 3.0, command=True), BARS, "command"),
                         {"push", "hold"})
        # Not a command: never a hold, whatever it holds.
        self.assertEqual(metrics.decisions(candidate("a", 3.0, command=True), BARS, "file"), {"push"})
        self.assertEqual(metrics.decisions(candidate("a", 2.9, command=True), BARS, "command"), {"push"})
        anchored_failure = {**BARS, "failure anchor": True}
        self.assertEqual(metrics.decisions(candidate("a", 1.0, fired=False, failed=True), anchored_failure,
                                           "command"), set())
        self.assertEqual(metrics.decisions(candidate("a", 1.0, fired=False, failed=True, anchored=True),
                                           anchored_failure, "command"), {"failure"})


class Places(unittest.TestCase):
    def test_a_passage_touching_a_point_does_not_overlap_it(self):
        point = {"start": 10, "end": 20}
        self.assertFalse(metrics.overlaps(candidate("a", 0, start=0, length=10), point))
        self.assertTrue(metrics.overlaps(candidate("a", 0, start=0, length=11), point))
        self.assertFalse(metrics.overlaps(candidate("a", 0, start=20, length=5), point))
        self.assertTrue(metrics.overlaps(candidate("a", 0, start=19, length=5), point))

    def test_whole_means_the_passage_covers_the_point(self):
        point = {"start": 10, "end": 20}
        self.assertTrue(metrics.holds_whole(candidate("a", 0, start=10, length=10), point))
        self.assertFalse(metrics.holds_whole(candidate("a", 0, start=11, length=10), point))
        self.assertFalse(metrics.holds_whole(candidate("a", 0, start=9, length=10), point))

    def test_a_context_passage_is_right_for_any_need_of_its_memory(self):
        needed = {"a": [{"start": 50, "end": 60}]}
        self.assertTrue(metrics.right(candidate("a", 0, slot=255, start=0, length=5), needed))
        self.assertFalse(metrics.right(candidate("a", 0, slot=1, start=0, length=5), needed))
        self.assertFalse(metrics.right(candidate("b", 0, slot=255), needed))


class Evaluate(unittest.TestCase):
    """A task of three actions, worked out by hand:
    - action 0 (prompt) needs memory "rule"; action 2 (command) needs
      point 1 of the long memory "book" and memory "lesson";
    - action 0 pushes "rule" (right) and "other" (wrong);
    - action 1 (file) pushes "rule" again (shown, so nothing) and a
      passage of "book" that overlaps its second point without holding all
      of it (right, early, not whole);
    - action 2 (command) holds "lesson" (right, on time) and holds "noise"
      (wrong); "book"'s first point passes too but is not needed.
    """

    memories = {
        "rule": {"shape": "rule", "points": [{"start": 0, "end": 30}]},
        "other": {"shape": "note", "points": [{"start": 0, "end": 30}]},
        "book": {"shape": "merged", "points": [{"start": 0, "end": 100}, {"start": 102, "end": 200}]},
        "lesson": {"shape": "lesson", "points": [{"start": 0, "end": 40}]},
        "noise": {"shape": "look-alike", "points": [{"start": 0, "end": 40}]},
    }
    task = {
        "actions": [{"kind": "prompt"}, {"kind": "file"}, {"kind": "command"}],
        "needs": [{"action": 0, "memory": "rule", "point": 0, "subject": "rule"},
                  {"action": 2, "memory": "book", "point": 1, "subject": "command"},
                  {"action": 2, "memory": "lesson", "point": 0, "subject": "failure"}],
    }
    matched = [
        [candidate("rule", 3.0, length=30), candidate("other", 2.5, length=30),
         candidate("book", 1.0, start=0, length=100)],
        [candidate("rule", 3.0, length=30), candidate("book", 2.2, slot=2, start=103, length=97)],
        [candidate("lesson", 3.5, length=40, command=True), candidate("noise", 3.1, length=40, command=True),
         candidate("book", 2.0, slot=1, start=0, length=100)],
    ]

    def setUp(self):
        self.counts = metrics.evaluate(self.task, self.matched, self.memories, BARS)

    def test_precision_counts_each_passage_once(self):
        c = self.counts
        # rule, other, book#2, lesson, noise, book#1: six shown, three right.
        self.assertEqual((c["delivered|all"], c["right|all"]), (6, 3))
        self.assertEqual((c["delivered|signal:prompt"], c["right|signal:prompt"]), (2, 1))
        self.assertEqual((c["delivered|way:hold"], c["right|way:hold"]), (2, 1))

    def test_recall_and_timing(self):
        c = self.counts
        self.assertEqual((c["needed|all"], c["found|all"]), (3, 3))
        self.assertEqual(c["timing on time|all"], 2)
        self.assertEqual(c["timing early|all"], 1)
        self.assertEqual(c["timing never|all"], 0)

    def test_holds_failures_and_whole_points(self):
        c = self.counts
        self.assertEqual((c["command needs"], c["held before its action"]), (2, 1))
        self.assertEqual((c["failure needs"], c["failure found at its action"]), (1, 1))
        self.assertEqual((c["merged delivered"], c["merged whole"]), (1, 0))

    def test_a_long_passage_not_needed_then_outranking_a_short_need_is_counted(self):
        c = self.counts
        # rule at action 0 and lesson at action 2, each above book's passage.
        self.assertEqual((c["short needed"], c["long over short"]), (2, 0))
        matched = [self.matched[0], self.matched[1],
                   [candidate("book", 4.0, slot=1, start=0, length=100),
                    candidate("lesson", 3.5, length=40, command=True)]]
        c = metrics.evaluate(self.task, matched, self.memories, BARS)
        self.assertEqual((c["short needed"], c["long over short"]), (2, 1))
        # The needed point of the long memory outranking it is no inversion.
        matched[2][0] = candidate("book", 4.0, slot=2, start=103, length=97)
        c = metrics.evaluate(self.task, matched, self.memories, BARS)
        self.assertEqual(c["long over short"], 0)

    def test_a_command_no_memory_names_counts_its_holds(self):
        task = {"actions": [{"kind": "prompt"}, {"kind": "command"}], "needs": []}
        matched = [[], [candidate("noise", 3.5, command=True)]]
        c = metrics.evaluate(task, matched, self.memories, BARS)
        self.assertEqual((c["unnamed commands"], c["unnamed held"]), (1, 1))
        c = metrics.evaluate(task, [[], [candidate("noise", 2.5, command=True)]], self.memories, BARS)
        self.assertEqual((c["unnamed commands"], c["unnamed held"]), (1, 0))
        # A command whose argument names another subcommand counts apart.
        task["actions"][1] = {"kind": "command", "collides": True}
        c = metrics.evaluate(task, matched, self.memories, BARS)
        self.assertEqual((c["colliding commands"], c["colliding held"], c["unnamed commands"]), (1, 1, 0))


class Classes(unittest.TestCase):
    """Why a delivery is wrong, why a need is missed, when a lesson shows."""

    git_tag = {"kind": "command", "about": ["command", "git", "tag"]}
    memories = {
        "note": {"shape": "note", "points": [{"start": 0, "end": 30, "about": ["command", "git", "tag"],
                                              "anchored": True}]},
        "decision": {"shape": "decision", "points": [{"start": 0, "end": 30,
                                                      "about": ["component", "parser"]}]},
        "start": {"shape": "start rule", "points": [{"start": 0, "end": 30, "about": ["work", "release"]}]},
        "lesson": {"shape": "lesson", "points": [{"start": 0, "end": 30, "about": ["command", "git", "tag"],
                                                  "anchored": True}]},
    }

    def test_a_wrong_delivery_is_about_this_action_or_about_something_else(self):
        about = metrics.about_action
        self.assertTrue(about(candidate("note", 3), self.memories["note"], self.git_tag, "change"))
        self.assertFalse(about(candidate("decision", 3), self.memories["decision"], self.git_tag, "change"))
        # A file and a design decision on one component are one subject.
        edit = {"kind": "file", "about": ["file", "parser"]}
        self.assertTrue(about(candidate("decision", 3), self.memories["decision"], edit, "change"))
        # A rule for a kind of work is about every action of a task of it.
        self.assertTrue(about(candidate("start", 3), self.memories["start"], self.git_tag, "release"))
        self.assertFalse(about(candidate("start", 3), self.memories["start"], self.git_tag, "review"))
        # A prompt naming a command is about that command too.
        prompt = {"kind": "prompt", "about": ["component", "parser"], "names": ["command", "git", "tag"]}
        self.assertTrue(about(candidate("note", 3), self.memories["note"], prompt, "change"))
        self.assertFalse(about(candidate("note", 3), self.memories["note"], {**prompt, "names": None}, "change"))
        # A passage beside the point is about nothing the point is.
        self.assertFalse(about(candidate("note", 3, start=30), self.memories["note"], self.git_tag, "change"))

    def test_a_missed_need_says_why_from_its_own_action(self):
        why = metrics.why_missed
        need = {"action": 0, "memory": "note", "point": 0}
        point = self.memories["note"]["points"][0]
        self.assertEqual(why(need, point, [[]], BARS, [self.git_tag]), metrics.NOT_MATCHED)
        self.assertEqual(why(need, point, [[candidate("note", 1, length=30)]], BARS, [self.git_tag]),
                         metrics.SPELLED)
        self.assertEqual(why(need, point, [[candidate("note", 1, length=30, anchored=True)]], BARS,
                             [self.git_tag]), metrics.BELOW)
        unanchored = self.memories["decision"]["points"][0]
        self.assertEqual(why({**need, "memory": "decision"}, unanchored, [[]], BARS, [self.git_tag]),
                         metrics.NO_ANCHOR)

    def test_a_delivery_is_placed_against_its_commands_runs(self):
        lesson = self.memories["lesson"]
        moment = lambda i, actions, way: metrics.run_moment(lesson, candidate("lesson", 3, length=30), i,
                                                           actions, way)
        prompt = {"kind": "prompt", "about": ["component", "parser"]}
        failing = [prompt, {**self.git_tag, "failed": True}, prompt, self.git_tag]
        self.assertEqual(moment(0, failing, "push"), metrics.BEFORE_FAILURE)
        self.assertEqual(moment(1, failing, "push"), metrics.AT_FAILURE)
        self.assertEqual(moment(2, failing, "push"), metrics.AFTER_FAILURE)
        # Against the last run before it: the second run succeeded.
        self.assertEqual(moment(3, failing, "push"), metrics.AFTER_SUCCESS)
        # A hold at the failing command shows it before the command runs.
        self.assertEqual(moment(1, failing, "hold"), metrics.BEFORE_FAILURE)
        passing = [prompt, self.git_tag]
        self.assertEqual(moment(0, passing, "push"), metrics.BEFORE_SUCCESS)
        self.assertEqual(moment(1, passing, "hold"), metrics.BEFORE_SUCCESS)
        self.assertEqual(moment(1, passing, "push"), metrics.AFTER_SUCCESS)
        self.assertEqual(moment(0, [prompt], "push"), metrics.NOT_RUN)
        decision = self.memories["decision"]
        self.assertEqual(metrics.run_moment(decision, candidate("decision", 3, length=30), 0, passing, "push"),
                         metrics.NO_COMMAND)

    def test_a_trap_is_met_by_a_hold_and_late_after_its_command(self):
        memories = {**self.memories, "trap": {"shape": "trap", "points": [
            {"start": 0, "end": 30, "about": ["command", "git", "tag"], "anchored": True}]}}
        task = {"kind": "release", "actions": [{"kind": "prompt"}, self.git_tag],
                "needs": [{"action": 1, "memory": "trap", "point": 0, "subject": "trap"}]}
        held = metrics.evaluate(task, [[], [candidate("trap", 3.5, length=30, command=True)]], memories, BARS)
        self.assertEqual((held["now|all"], held["late|all"], held["timing on time|all"]), (1, 0, 1))
        self.assertEqual((held["command needs"], held["held before its action"]), (1, 1))
        pushed = metrics.evaluate(task, [[], [candidate("trap", 2.5, length=30, command=True)]], memories, BARS)
        # Right in the task, so never a wrong topic; late, as the command ran.
        self.assertEqual((pushed["right|all"], pushed["now|all"], pushed["late|all"]), (1, 0, 1))
        self.assertEqual((pushed["timing late|all"], pushed["in time|all"], pushed["found|all"]), (1, 0, 1))
        self.assertEqual(pushed[f"moment {metrics.AFTER_SUCCESS}|shape:trap|late"], 1)
        # A note pushed at its command is on time: it holds whatever the run does.
        task["needs"][0] = {"action": 1, "memory": "note", "point": 0, "subject": "command"}
        note = metrics.evaluate(task, [[], [candidate("note", 2.5, length=30, command=True)]], memories, BARS)
        self.assertEqual((note["now|all"], note["timing on time|all"]), (1, 1))

    def test_an_event_pushes_at_most_three_and_the_rest_wait(self):
        memories = {m: {"shape": "note", "points": [{"start": 0, "end": 10}]} for m in "abcde"}
        task = {"kind": "change", "actions": [{"kind": "file"}, {"kind": "file"}],
                "needs": [{"action": 1, "memory": m, "point": 0, "subject": "file"} for m in "abcde"]}
        five = [candidate(m, 7 - n) for n, m in enumerate("abcde")]
        c = metrics.evaluate(task, [five, []], memories, BARS)
        self.assertEqual(c["delivered|all"], metrics.PUSH_PER_EVENT)
        self.assertEqual(c["found|all"], 3)
        # The two the store ranked lowest come at the next action that matches them.
        c = metrics.evaluate(task, [five, five], memories, BARS)
        self.assertEqual((c["delivered|all"], c["found|all"], c["timing on time|all"]), (5, 5, 2))
        # Holds are not pushes and do not count against it.
        holds = [candidate(m, 5, command=True) for m in "abcde"]
        task["actions"] = [{"kind": "command"}]
        task["needs"] = []
        c = metrics.evaluate(task, [holds], memories, BARS)
        self.assertEqual((c["delivered|way:hold"], c["denials"]), (5, 1))
        task["actions"] = [{"kind": "file"}]
        c = metrics.evaluate(task, [five], memories, {**BARS, "per_event": None})
        self.assertEqual(c["delivered|all"], 5)

    def test_a_hold_cap_pushes_or_drops_the_rest_and_sizes_are_counted(self):
        memories = {m: {"shape": "note", "points": [{"start": 0, "end": 10}]} for m in "abcde"}
        task = {"kind": "change", "actions": [{"kind": "command"}],
                "needs": [{"action": 0, "memory": "a", "point": 0, "subject": "command"}]}
        holds = [[candidate(m, 7 - n, command=True) for n, m in enumerate("abcde")]]
        c = metrics.evaluate(task, holds, memories, BARS)
        self.assertEqual((c["hold size 5"], c["denials"], c["denials with a need"]), (1, 1, 1))
        dropped = metrics.evaluate(task, holds, memories, {**BARS, "hold cap": 3})
        self.assertEqual((dropped["delivered|way:hold"], dropped["delivered|way:push"]), (3, 0))
        pushed = metrics.evaluate(task, holds, memories, {**BARS, "hold cap": 3, "hold rest": "push"})
        self.assertEqual((pushed["delivered|way:hold"], pushed["delivered|way:push"]), (3, 2))
        # A denial carrying nothing needed now.
        task["needs"] = []
        c = metrics.evaluate(task, holds, memories, BARS)
        self.assertEqual((c["denials"], c["denials with a need"]), (1, 0))

    def test_a_session_remembers_what_it_showed(self):
        memories = {"a": {"shape": "note", "points": [{"start": 0, "end": 10}]}}
        task = {"kind": "change", "actions": [{"kind": "command"}],
                "needs": [{"action": 0, "memory": "a", "point": 0, "subject": "command"}]}
        held = [[candidate("a", 5, command=True)]]
        session = set()
        metrics.evaluate(task, held, memories, BARS, session)
        again = metrics.evaluate(task, held, memories, BARS, session)
        self.assertEqual((again["held again"], again["denials"]), (1, 1))
        remembered = metrics.evaluate(task, held, memories, {**BARS, "remember": True}, session)
        self.assertEqual((remembered["denials"], remembered["delivered|all"]), (0, 0))
        # The need is met, as seen in an earlier task of the session.
        self.assertEqual((remembered["found|all"], remembered["timing early|all"]), (1, 1))

    def test_the_lesson_oracle_shows_a_lesson_only_after_its_command_failed(self):
        tag = ["command", "git", "tag"]
        memories = {"lesson": {"shape": "lesson", "points": [{"start": 0, "end": 10, "about": tag}]}}
        oracle = {**BARS, "lesson oracle": True}
        for failed, way, shown in ((False, "push", 0), (True, "push", 1), (True, "hold", 0)):
            task = {"kind": "release", "actions": [{"kind": "command", "about": tag, "failed": failed}],
                    "needs": []}
            score = 2.5 if way == "push" else 3.5
            c = metrics.evaluate(task, [[candidate("lesson", score, command=True)]], memories, oracle)
            self.assertEqual(c["delivered|all"], shown, (failed, way))

    def test_deliveries_and_needs_are_banded_by_their_anchors_holders(self):
        memories = {m: {"shape": "note", "points": [{"start": 0, "end": 10, "anchored": True}]}
                    for m in ("one", "few", "many", "words")}
        memories["words"]["points"][0]["anchored"] = False
        task = {"kind": "change", "actions": [{"kind": "file"}],
                "needs": [{"action": 0, "memory": m, "point": 0, "subject": "file"} for m in memories]}
        matched = [[candidate("one", 5, fewest=1), candidate("few", 4, fewest=4)]]
        c = metrics.evaluate(task, matched, memories, BARS)
        self.assertEqual((c["band 1|delivered"], c["band 3-5|delivered"], c["band 3-5|now"]), (1, 1, 1))
        self.assertEqual((c["needed|band 1"], c["found|band 3-5"]), (1, 1))
        # An anchored need its action never matched, and one with no anchor.
        self.assertEqual(c["needed|band not a candidate"], 1)
        self.assertEqual(sum(c[f"needed|band {b}"] for b in metrics.BANDS + ["not a candidate"]), 3)
        self.assertEqual([metrics.band(n) for n in (0, 1, 2, 3, 5, 6, 40)],
                         ["no anchor", "1", "2", "3-5", "3-5", "6+", "6+"])

    def test_rules_that_name_no_action_stay_out_of_the_totals(self):
        task = {"kind": "release",
                "actions": [{"kind": "prompt", "literal": True}, self.git_tag],
                "needs": [{"action": 0, "memory": "start", "point": 0, "subject": "start rule"},
                          {"action": 1, "memory": "note", "point": 0, "subject": "command"}]}
        matched = [[candidate("start", 3, length=30)], [candidate("decision", 3, length=30, anchored=True)]]
        c = metrics.evaluate(task, matched, self.memories, BARS)
        self.assertEqual((c["delivered|all"], c["right|all"], c["wrong elsewhere|all"]), (1, 0, 1))
        self.assertEqual((c["delivered|shape:start rule"], c["right|shape:start rule"]), (1, 1))
        self.assertEqual((c["needed|all"], c["needed|subject:start rule"]), (1, 1))
        self.assertEqual(c[f"missed {metrics.NOT_MATCHED}|all"], 1)
        self.assertEqual((c["prompts"], c["prompts literal"], c["delivered|prompt literal"]), (1, 1, 0))


class Generator(unittest.TestCase):
    def test_the_same_seed_gives_the_same_world(self):
        self.assertEqual(world.generate(5, 50), world.generate(5, 50))
        self.assertNotEqual(world.generate(5, 50), world.generate(6, 50))

    def test_a_world_fills_its_size_and_every_need_names_a_memory_and_point(self):
        drawn = world.generate(5, 1_000)
        self.assertEqual(len(drawn["memories"]), 1_000)
        memories = {m["id"]: m for m in drawn["memories"]}
        for task in drawn["tasks"]:
            for need in task["needs"]:
                memory = memories[need["memory"]]
                point = memory["points"][need["point"]]
                self.assertLess(point["start"], point["end"])
                self.assertLessEqual(point["end"], len(memory["content"]))
                self.assertLess(need["action"], len(task["actions"]))

    def test_no_look_alike_is_about_a_subject_a_task_needs(self):
        drawn = world.generate(5, 1_000)
        run = {f"`{p} {s}`" for commands in world.WORK.values() for p, s in commands}
        for memory in drawn["memories"]:
            if memory["shape"] == "look-alike":
                self.assertFalse(any(memory["content"].startswith(c) for c in run), memory)

    def test_a_stated_share_of_actions_spell_their_subject_another_way(self):
        drawn = world.generate(5, 1_000)
        actions = [a for t in drawn["tasks"] for a in t["actions"] if a["kind"] in ("command", "file")
                   and a.get("about")]
        spelled = [a for a in actions if a.get("spelled")]
        self.assertTrue(0.1 < len(spelled) / len(actions) < 0.3, (len(spelled), len(actions)))
        for a in spelled:
            if a["spelled"] in ("alias", "wrapper"):
                program, sub = a["about"][1:]
                self.assertNotIn(f"{program} {sub}", a["text"], a)
            elif a["spelled"] == "flag first":
                self.assertIn(world.FLAG_FIRST[a["about"][1]], a["text"], a)
            elif a["spelled"] == "prefix":
                self.assertTrue(a["text"].startswith(tuple(world.PREFIXES)), a)
                self.assertIn(" ".join(a["about"][1:]), a["text"], a)
            else:
                self.assertTrue(a["text"].startswith(("./", world.ROOT_FOLDER)), a)
        self.assertEqual({a["spelled"] for a in spelled} & {"alias", "wrapper", "relative"},
                         {"alias", "wrapper", "relative"})

    def test_a_share_of_tasks_run_a_command_naming_another_subcommand(self):
        drawn = world.generate(5, 1_000)
        colliding = [a for t in drawn["tasks"] for a in t["actions"] if a.get("collides")]
        self.assertTrue(colliding)
        subcommands = {s for _, s in world.COMMANDS if s}
        for a in colliding:
            self.assertNotIn("about", a)
            self.assertTrue(set(a["text"].split()) & subcommands, a)

    def test_tasks_no_memory_names_hold_only_unnamed_commands(self):
        drawn = world.generate(5, 1_000)
        unnamed = [t for t in drawn["tasks"] if t["unnamed"]]
        self.assertTrue(unnamed)
        for task in unnamed:
            self.assertEqual([n["action"] for n in task["needs"]], [0] * len(task["needs"]))
        kinds = Counter(t["kind"] for t in drawn["tasks"])
        self.assertEqual(set(kinds), set(world.KINDS))


if __name__ == "__main__":
    unittest.main()
