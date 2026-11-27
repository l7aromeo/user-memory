"""Tests of the delivery benchmark's measures and generator, on cases
worked out by hand.

    python3 -I evals/delivery/test_bench.py
"""

import json
import sys
import tempfile
import unittest
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import metrics  # noqa: E402
import run  # noqa: E402
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


class Combined(unittest.TestCase):
    def test_a_command_and_other_events_are_tested_on_their_own_quantities(self):
        c = {**candidate("a", 0, anchored=True, command=True), "share": 0.9, "mixed": 0.1}
        bars = {"scale": "kind", "anchor": True,
                "command": {"scale": "share", "push": 0.6, "failure": 0.6, "hold": 0.75},
                "other": {"scale": "mixed", "push": 0.4, "failure": 0.4, "hold": 0.4}}
        self.assertEqual(metrics.decisions(c, bars, "command"), {"push", "hold"})
        self.assertEqual(metrics.decisions(c, bars, "file"), set())

    def test_either_passes_a_way_that_any_part_passes(self):
        c = {**candidate("a", 0, anchored=True, command=True), "share": 0.65, "mixed": 0.5}
        bars = {"scale": "either", "anchor": True,
                "parts": [{"scale": "share", "push": 0.6, "failure": 0.6, "hold": 0.75},
                          {"scale": "mixed", "push": 0.4, "failure": 0.4, "hold": 0.6}]}
        self.assertEqual(metrics.decisions(c, bars, "command"), {"push"})
        c["mixed"] = 0.6
        self.assertEqual(metrics.decisions(c, bars, "command"), {"push", "hold"})


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
        # A second run of a busy command in the task holds nothing again.
        task["actions"] = [{"kind": "prompt"}, {"kind": "command", "busy": True},
                           {"kind": "command", "busy": True, "repeat": 1}]
        c = metrics.evaluate(task, [[], matched[1], matched[1]], self.memories, BARS)
        self.assertEqual((c["repeat runs"], c["repeat held"], c["repeat nothing"], c["busy runs"]), (1, 0, 1, 2))
        self.assertEqual((c["busy delivered"], c["busy delivered now"]), (1, 0))


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
        # A lesson that would hold the failing command is pushed after it.
        for failed, way, shown in ((False, "push", 0), (True, "push", 1), (True, "hold", 1), (False, "hold", 0)):
            task = {"kind": "release", "actions": [{"kind": "command", "about": tag, "failed": failed}],
                    "needs": []}
            score = 2.5 if way == "push" else 3.5
            c = metrics.evaluate(task, [[candidate("lesson", score, command=True)]], memories, oracle)
            self.assertEqual((c["delivered|all"], c["delivered|way:hold"]), (shown, 0), (failed, way))

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
        together = ("chained", "read with others")
        spelled = [a for a in actions if a.get("spelled") and a["spelled"] not in together]
        self.assertTrue(0.1 < len(spelled) / len(actions) < 0.3, (len(spelled), len(actions)))
        for a in spelled:
            if a["spelled"] in ("alias", "wrapper"):
                program, sub = a["about"][1:]
                self.assertNotIn(f"{program} {sub}", a["text"], a)
            elif a["spelled"] in ("flag first", "flag with a word value"):
                self.assertEqual(a["spelled"] == "flag with a word value", a["about"][1] in world.WORD_VALUED, a)
                self.assertIn(world.FLAG_FIRST[a["about"][1]], a["text"], a)
            elif a["spelled"] == "prefix":
                self.assertTrue(a["text"].startswith(tuple(world.PREFIXES)), a)
                self.assertIn(" ".join(a["about"][1:]), a["text"], a)
            else:
                self.assertTrue(a["text"].startswith(("./", world.ROOT_FOLDER)), a)
        self.assertEqual({a["spelled"] for a in spelled} & {"alias", "wrapper", "relative"},
                         {"alias", "wrapper", "relative"})

    def test_a_stated_share_of_commands_and_reads_run_in_a_line_with_others(self):
        # Pooled over seeds: one world runs too few commands for a share.
        actions = [a for seed in range(1, 9) for t in world.generate(seed, 50)["tasks"]
                   for a in t["actions"] if a.get("about")]
        commands = [a for a in actions if a["about"][0] == "command"]
        chained = [a for a in commands if a.get("spelled") == "chained"]
        # Of the first runs spelled as written, CHAINED_SHARE; a repeat
        # runs its first line again.
        written = [a for a in commands if not a.get("repeat") and a.get("spelled") in (None, "chained")]
        self.assertAlmostEqual(len(chained) / len(written), world.CHAINED_SHARE, delta=0.1)
        for a in chained:
            self.assertEqual(a["kind"], "command")
            self.assertIn(" ".join(p for p in a["about"][1:] if p), a["text"], a)
            self.assertRegex(a["text"], r"&&|;|\|", a)
        reads = [a for a in actions if a.get("spelled") == "read with others"]
        self.assertTrue(reads)
        for a in reads:
            self.assertEqual(a["kind"], "command")
            program, *paths = a["text"].split()
            own = [path for path in paths if path not in world.READ_BESIDE]
            self.assertEqual(program, "cat", a)
            self.assertTrue(len(paths) >= 2 and len(own) == 1, a)
            self.assertIn(f"/{a['about'][1]}.", own[0], a)

    def test_a_share_of_tasks_run_a_command_naming_another_subcommand(self):
        drawn = world.generate(5, 1_000)
        colliding = [a for t in drawn["tasks"] for a in t["actions"] if a.get("collides")]
        self.assertTrue(colliding)
        subcommands = {s for _, s in world.COMMANDS if s}
        self.assertEqual({a["collides"] for a in colliding}, {"plain", "after a flag"})
        for a in colliding:
            self.assertNotIn("about", a)
            self.assertTrue(set(a["text"].split()) & subcommands, a)

    def test_a_busy_command_runs_again_and_needs_a_share_of_its_notes(self):
        base = world.generate(5, 1_000)
        saved = world.BUSY_NEEDED, world.BUSY_REPEATS
        try:
            world.BUSY_NEEDED, world.BUSY_REPEATS = 0.25, 3
            varied = world.generate(5, 1_000)
        finally:
            world.BUSY_NEEDED, world.BUSY_REPEATS = saved
        # The memories drawn before the tasks are the same; filler after them differs.
        drawn_first = ("note", "trap", "lesson", "decision", "rule", "start rule", "essential", "merged")
        structured = lambda drawn: sorted(m["id"] for m in drawn["memories"] if m["shape"] in drawn_first)
        self.assertEqual(structured(base), structured(varied))
        runs = [a for t in varied["tasks"] for a in t["actions"] if a.get("busy")]
        repeats = [a for a in runs if a["repeat"]]
        self.assertEqual(len(repeats), 2 * (len(runs) - len(repeats)))
        # A run needs fewer of the busy notes than every one of them:
        # counted at the busy runs, as the world's other commands would
        # dilute a count over every action.
        def per_run(drawn):
            runs = needs = 0
            for t in drawn["tasks"]:
                at = [n["action"] for n in t["needs"]]
                for i, a in enumerate(t["actions"]):
                    if a.get("busy"):
                        runs += 1
                        needs += at.count(i)
            return needs / runs
        # A quarter of them needed: well under three quarters of the
        # notes per run, where needing all of them would be about equal.
        self.assertLess(per_run(varied), 0.75 * per_run(base))

    def test_tasks_no_memory_names_hold_only_unnamed_commands(self):
        drawn = world.generate(5, 1_000)
        unnamed = [t for t in drawn["tasks"] if t["unnamed"]]
        self.assertTrue(unnamed)
        for task in unnamed:
            self.assertEqual([n["action"] for n in task["needs"]], [0] * len(task["needs"]))
        kinds = Counter(t["kind"] for t in drawn["tasks"])
        self.assertEqual(set(kinds), set(world.KINDS))


class SessionShare(unittest.TestCase):
    def test_each_round_is_its_own_share_so_a_slow_round_moves_nothing(self):
        # Every round adds a quarter of its build; one runs three times slower.
        rounds = [{"alone": 40.0, "building": 90.0, "finding": 80.0} for _ in range(10)]
        rounds.append({"alone": 120.0, "building": 270.0, "finding": 240.0})
        share, low, high, steps = run.session_share(rounds)
        self.assertAlmostEqual(share, 0.25)
        self.assertAlmostEqual(low, 0.25)
        self.assertAlmostEqual(high, 0.25)
        self.assertEqual(steps, {"alone": 40.0, "building": 90.0, "finding": 80.0})

    def test_the_share_is_the_median_of_the_rounds_and_the_middle_half_their_quartiles(self):
        # Shares 10, 90, 20, 30 and 50 %; the difference of the steps'
        # medians, 160 - 140 ms, would give 20 %.
        pairs = [(150, 140), (300, 210), (120, 100), (250, 220), (160, 110)]
        rounds = [{"alone": 100.0, "building": float(b), "finding": float(f)} for b, f in pairs]
        share, low, high, _ = run.session_share(rounds)
        self.assertAlmostEqual(share, 0.3)
        self.assertAlmostEqual(low, 0.2)
        self.assertAlmostEqual(high, 0.5)

    def test_a_build_awaited_in_full_is_over_the_bar(self):
        rounds = [{"alone": 40.0, "building": 120.0, "finding": 80.0} for _ in range(run.SESSION_ROUNDS)]
        self.assertGreater(run.session_share(rounds)[0], run.SESSION_BUILD_SHARE)


def shaped(shares):
    """Rounds of a 40 ms build whose shares are `shares`."""
    return [{"alone": 40.0, "building": 80.0 + 40.0 * share, "finding": 80.0} for share in shares]


# Rounds as #246's Windows run had them: a middle half from 25 % to about
# 1,500 %, so half the rounds are over the bar on a runner whose speed
# swung within each round.
SWUNG = shaped([0.1, 0.2, 0.25, 0.3, 0.5, 0.8, 0.9, 1.0, 1.02, 1.1, 2.0,
                3.0, 5.0, 9.0, 14.0, 14.7, 15.0, 15.5, 16.0, 18.0, 20.0][:run.SESSION_ROUNDS])


class SessionJudged(unittest.TestCase):
    """`session_judged`: a set of rounds too spread to judge is timed once
    more, and a second one as spread is called too noisy, not a failure."""

    def judged(self, *sets):
        taken = list(sets)
        timed = run.session_judged(lambda: taken.pop(0))
        return timed, run.session_verdict(timed)

    def test_steady_rounds_under_the_bar_are_timed_once_and_pass(self):
        timed, (share, _, _, _, noisy) = self.judged(shaped([0.2, 0.25, 0.3] * 7))
        self.assertEqual(len(timed), 1)
        self.assertAlmostEqual(share, 0.25)
        self.assertFalse(noisy)

    def test_a_build_awaited_in_full_is_judged_at_once_and_over_the_bar(self):
        # #225's awaited build: a middle half from 100 % to 106 %.
        timed, (share, _, _, _, noisy) = self.judged(shaped([1.0, 1.03, 1.06] * 7))
        self.assertEqual(len(timed), 1)
        self.assertGreater(share, run.SESSION_BUILD_SHARE)
        self.assertFalse(noisy)

    def test_a_swung_set_is_timed_again_and_the_second_judged(self):
        timed, (share, _, _, _, noisy) = self.judged(SWUNG, shaped([0.2, 0.25, 0.3] * 7))
        self.assertEqual(len(timed), 2)
        self.assertAlmostEqual(share, 0.25)
        self.assertFalse(noisy)

    def test_two_swung_sets_are_too_noisy_to_judge(self):
        timed, (share, low, high, _, noisy) = self.judged(SWUNG, SWUNG)
        self.assertEqual(len(timed), 2)
        self.assertTrue(noisy)
        self.assertGreater(share, run.SESSION_BUILD_SHARE, "a judged run would fail")
        self.assertGreater(high - low, run.SESSION_NOISY_SPREAD)

    def test_the_widest_calibration_run_is_judged(self):
        # #225's widest run at 21 rounds: Windows x64, -87 % to 50 %.
        wide = shaped([-1.5, -1.4, -1.3, -1.2, -1.0, -0.87, -0.6, -0.4, -0.2, 0.0, 0.1,
                       0.2, 0.3, 0.4, 0.45, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0])
        timed, (_, low, high, _, noisy) = self.judged(wide)
        self.assertAlmostEqual(low, -0.87)
        self.assertAlmostEqual(high, 0.5)
        self.assertEqual(len(timed), 1, f"middle half {low:.0%} to {high:.0%}")
        self.assertFalse(noisy)


def added(times, start=40.0):
    """A hook line's pairs: each run adds `times[n]` ms over a `start` ms
    start."""
    return [(start + took, start) for took in times]


# Ten runs adding about 12 ms over their start, steady.
STEADY = added([11.0, 12.0, 13.0, 12.0, 11.5, 12.5, 12.0, 13.0, 11.0, 12.0])
# Ten runs adding from nothing to 120 ms: a runner whose speed swung
# within the pairs themselves.
SPREAD = added([0.0, 5.0, 10.0, 20.0, 40.0, 60.0, 80.0, 100.0, 110.0, 120.0])


class HookJudged(unittest.TestCase):
    """`hooks_judged` and `hook_verdict` (#284): a hook line is judged on
    what the hook adds over a start that returns at once, a set too spread
    to judge is timed once more, and a second as spread is too noisy, not
    a failure."""

    def judged(self, *sets, line="tool", limit=100):
        taken = [{line: timed if isinstance(timed, dict) else run.hook_set(timed)}
                 for timed in sets]
        asked = []

        def take(again):
            asked.append(again)
            return taken.pop(0)

        judged = run.hooks_judged(take, {line: limit})
        return asked, judged[line], run.hook_verdict(judged[line], limit)

    def test_a_slowdown_added_to_every_run_fails(self):
        asked, sets, verdict = self.judged([(took + 100.0, base) for took, base in STEADY])
        self.assertEqual(asked, [False])
        self.assertGreater(verdict["added"], 100)
        self.assertFalse(verdict["noisy"])
        self.assertTrue(verdict["over"])

    def test_a_slow_stretch_on_both_of_its_pairs_passes(self):
        # Six runs in a stretch 80 ms slower, hook and start alike: the
        # hooks' median is over the limit, what they add is not.
        slow = [(took + 80.0, base + 80.0) if 2 <= n < 8 else (took, base)
                for n, (took, base) in enumerate(STEADY)]
        asked, sets, verdict = self.judged(slow)
        self.assertEqual(asked, [False])
        self.assertGreater(verdict["hook"], 100, "the hook's own median would fail")
        self.assertAlmostEqual(verdict["added"], 12.0)
        self.assertFalse(verdict["over"])

    def test_a_spread_set_is_timed_again_and_the_second_judged(self):
        asked, sets, verdict = self.judged(SPREAD, STEADY)
        self.assertEqual(asked, [False, True])
        self.assertEqual(len(sets), 2)
        self.assertAlmostEqual(verdict["added"], 12.0)
        self.assertFalse(verdict["noisy"])
        self.assertFalse(verdict["over"])

    def test_a_run_out_of_its_time_is_timed_again_and_the_second_judged(self):
        asked, sets, verdict = self.judged(run.hook_set(STEADY[1:], [(140.0, 40.0)]), STEADY)
        self.assertEqual(asked, [False, True])
        self.assertAlmostEqual(verdict["added"], 12.0)
        self.assertFalse(verdict["noisy"])
        self.assertFalse(verdict["over"])

    def test_runs_out_of_their_time_twice_are_too_noisy_and_not_failed(self):
        ran_out = run.hook_set(STEADY[1:], [(140.0, 40.0)])
        asked, sets, verdict = self.judged(ran_out, ran_out)
        self.assertEqual(asked, [False, True])
        self.assertTrue(verdict["noisy"])
        self.assertFalse(verdict["over"])

    def test_two_spread_sets_are_too_noisy_to_judge_and_not_failed(self):
        over = added([0.0, 5.0, 10.0, 20.0, 105.0, 110.0, 160.0, 170.0, 180.0, 190.0])
        asked, sets, verdict = self.judged(over, over)
        self.assertEqual(asked, [False, True])
        self.assertGreater(verdict["added"], 100, "a judged set would fail")
        self.assertTrue(verdict["noisy"])
        self.assertFalse(verdict["over"])

    def test_only_a_line_too_spread_takes_its_second_set(self):
        taken = [{"tool": run.hook_set(STEADY), "push": run.hook_set(SPREAD)},
                 {"tool": run.hook_set(SPREAD), "push": run.hook_set(STEADY)}]
        sets = run.hooks_judged(lambda again: taken.pop(0), {"tool": 100, "push": 100})
        self.assertEqual([len(sets["tool"]), len(sets["push"])], [1, 2])
        self.assertEqual(sets["tool"], [run.hook_set(STEADY)])
        self.assertEqual(sets["push"][-1], run.hook_set(STEADY))

    def test_a_slower_start_passes_the_hook_lines_and_fails_the_start_line(self):
        # Every start, and so every hook run, 100 ms slower: what the hooks
        # add is unchanged, and the start line is what catches it.
        slower = {line: [run.hook_set([(took + 100.0, base + 100.0) for took, base in STEADY])]
                  for line in run.HOOK_LINES}
        limits = run.hook_limits(100, 250, 1000)
        for line in run.HOOK_LINES:
            self.assertFalse(run.hook_verdict(slower[line], limits[line])["over"], line)
        self.assertEqual(run.start_median(slower), 140.0)
        self.assertGreater(run.start_median(slower), run.START_LIMIT_MS)
        steady = {line: [run.hook_set(STEADY)] for line in run.HOOK_LINES}
        self.assertEqual(run.start_median(steady), 40.0)
        self.assertLess(run.start_median(steady), run.START_LIMIT_MS)

    def test_a_run_that_names_nothing_ran_out_of_time_only_at_or_past_the_limit(self):
        missed = []
        self.assertIs(run.push_checked("pushes abc12345", "abc12345", 60.0, 40.0, 100, missed), True)
        self.assertIsNone(run.push_checked("", "abc12345", 139.9, 40.0, 100, missed),
                          "a push that should have come within the limit")
        self.assertEqual(missed, [])
        self.assertIs(run.push_checked("", "abc12345", 140.0, 40.0, 100, missed), False)
        self.assertIs(run.push_checked("other", "abc12345", 400.0, 40.0, 100, missed), False)
        self.assertEqual(missed, [(140.0, 40.0), (400.0, 40.0)])

    def test_a_pair_alternates_which_runs_first(self):
        calls = []

        def hook_said(event, session, cwd, **extra):
            calls.append(session)
            return (1.0, "out") if session else (0.5, "")

        for n in range(4):
            self.assertEqual(run.pair(hook_said, n, "PostToolUse", "s0", "."), (1.0, "out", 0.5))
        self.assertEqual(calls, [None, "s0", "s0", None, None, "s0", "s0", None])


class SessionRounds(unittest.TestCase):
    def test_the_order_rotates_and_a_building_start_finds_no_base(self):
        steps, saved = [], run.bench
        with tempfile.TemporaryDirectory() as tmp:
            delivery = Path(tmp)
            (delivery / "store.lock").write_text("")

            def bench(_binary, *args):
                steps.append("alone")
                return json.dumps({"build_ms": 40.0})

            def start(session):
                files = sorted(p.name for p in delivery.iterdir())
                if session.startswith("building"):
                    self.assertEqual(files, ["store.lock"], session)
                else:
                    self.assertIn("store-scope.index", files, session)
                steps.append(session.rstrip("0123456789"))
                (delivery / "store-scope.index").write_text("")
                return 90.0 if session.startswith("building") else 80.0

            run.bench = bench
            try:
                rounds = run.session_rounds("bench", "db", delivery / "alone.index", delivery, start)
            finally:
                run.bench = saved
        self.assertEqual(len(rounds), run.SESSION_ROUNDS)
        self.assertEqual(rounds[0], {"alone": 40.0, "building": 90.0, "finding": 80.0})
        orders = [tuple(steps[3 * n:3 * n + 3]) for n in range(run.SESSION_ROUNDS)]
        self.assertEqual(orders[:3], [("alone", "building", "finding"), ("building", "finding", "alone"),
                                      ("finding", "alone", "building")])
        self.assertEqual({o[0] for o in orders}, {"alone", "building", "finding"})


class Scripts(unittest.TestCase):
    def test_a_script_run_is_grouped_by_how_it_is_invoked(self):
        def how(text, program="tools/release.sh"):
            return metrics.invocation({"kind": "command", "about": ["command", program, ""], "text": text})
        self.assertEqual(how("tools/release.sh"), ["bare"])
        self.assertEqual(how("./tools/release.sh --dry-run"), ["./", "with flags"])
        self.assertEqual(how("bash tools/release.sh"), ["bash or sh"])
        self.assertEqual(how("python3 tools/migrate.py --check", "tools/migrate.py"),
                         ["python3", "with flags"])
        # What a chain runs beside the script adds no flags to it.
        self.assertEqual(how("cd ledger && tools/release.sh 2>&1 | tail -20"), ["bare"])
        self.assertEqual(how("echo start; ./tools/release.sh; echo done"), ["./"])
        self.assertEqual(how("cargo build", "cargo"), [])
        self.assertEqual(metrics.invocation({"kind": "file", "text": "tools/release.sh"}), [])

    def test_a_script_runs_deliveries_and_needs_are_counted_by_its_invocation(self):
        memories = {"note": {"shape": "note", "points": [{"start": 0, "end": 40}]},
                    "noise": {"shape": "look-alike", "points": [{"start": 0, "end": 40}]}}
        task = {"actions": [{"kind": "prompt"},
                            {"kind": "command", "about": ["command", "tools/release.sh", ""],
                             "text": "bash tools/release.sh --fast"}],
                "needs": [{"action": 1, "memory": "note", "point": 0, "subject": "command"}]}
        matched = [[], [candidate("note", 3.5, length=40, command=True),
                        candidate("noise", 3.1, length=40, command=True)]]
        c = metrics.evaluate(task, matched, memories, BARS)
        for group in ("script bash or sh", "script with flags"):
            self.assertEqual((c[f"delivered|{group}"], c[f"right|{group}"]), (2, 1), group)
            self.assertEqual((c[f"needed|{group}"], c[f"found|{group}"]), (1, 1), group)
        self.assertEqual(c["delivered|script bare"], 0)


if __name__ == "__main__":
    unittest.main()
