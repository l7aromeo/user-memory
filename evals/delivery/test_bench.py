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
              anchored=False, command=False):
    return {"id": memory, "slot": slot, "start": start, "length": length, "score": score,
            "fired": fired, "failed": failed, "anchored": anchored, "command": command}


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

    def test_a_lesson_is_placed_against_its_commands_runs(self):
        moment = metrics.lesson_moment
        lesson = self.memories["lesson"]
        prompt = {"kind": "prompt", "about": ["component", "parser"]}
        failing = [prompt, {**self.git_tag, "failed": True}, self.git_tag]
        self.assertEqual(moment(lesson, 0, failing, "push"), metrics.BEFORE_FAILURE)
        self.assertEqual(moment(lesson, 1, failing, "push"), metrics.AT_FAILURE)
        self.assertEqual(moment(lesson, 2, failing, "push"), metrics.AFTER_FAILURE)
        # A hold at the failing command shows it before the command runs.
        self.assertEqual(moment(lesson, 1, failing, "hold"), metrics.BEFORE_FAILURE)
        passing = [prompt, self.git_tag]
        self.assertEqual(moment(lesson, 0, passing, "push"), metrics.BEFORE_SUCCESS)
        self.assertEqual(moment(lesson, 1, passing, "hold"), metrics.BEFORE_SUCCESS)
        self.assertEqual(moment(lesson, 1, passing, "push"), metrics.AFTER_SUCCESS)
        self.assertEqual(moment(lesson, 0, [prompt], "push"), metrics.NOT_RUN)

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
