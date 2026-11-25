#!/usr/bin/env python3
"""Checks of the retrieval benchmark: its metrics on hand-computed ranks,
its cases and the fixtures' `supersedes` field. Run with
`python3 -I evals/retrieval/test_run.py`."""

import importlib.util
import json
import math
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run", HERE / "run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)
BINARY = run.ROOT / "target/debug/user-memory"


def row(rank, **extra):
    """A result row whose target `t` is at `rank` (None: missing)."""
    found = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]
    if rank is not None:
        found[rank - 1] = "t"
    return {"style": "direct", "rank": rank, "ndcg": run.ndcg(found, {"t": 2}),
            "matched": len(found), "bytes": 100, "found": found, **extra}


class Metrics(unittest.TestCase):
    def test_ndcg_is_one_with_the_target_first(self):
        self.assertEqual(run.ndcg(["t", "x"], {"t": 2}), 1.0)

    def test_ndcg_discounts_a_lone_target_by_its_rank(self):
        # (2^2 - 1) / log2(3 + 1) over (2^2 - 1) / log2(2).
        self.assertAlmostEqual(run.ndcg(["x", "y", "t"], {"t": 2}), 0.5)

    def test_ndcg_with_a_related_memory_above_the_target(self):
        # DCG: 1/log2(2) + 3/log2(3); ideal: 3/log2(2) + 1/log2(3).
        got = run.ndcg(["r", "t"], {"t": 2, "r": 1})
        self.assertAlmostEqual(got, (1 + 3 / math.log2(3)) / (3 + 1 / math.log2(3)))

    def test_ndcg_counts_only_the_first_ten_ranks(self):
        found = [str(n) for n in range(10)] + ["t"]
        self.assertEqual(run.ndcg(found, {"t": 2}), 0.0)
        self.assertGreater(run.ndcg(found[1:], {"t": 2}), 0.0)

    def test_ndcg_of_a_miss_is_zero(self):
        self.assertEqual(run.ndcg(["x"], {"t": 2, "r": 1}), 0.0)

    def test_grades_give_the_target_two_and_related_one(self):
        self.assertEqual(run.grades({"target": "t", "related": ["r"]}), {"t": 2, "r": 1})

    def test_score_counts_ranks_at_each_depth(self):
        rows = [row(1), row(3), row(5), row(6), row(None)]
        s = run.score(rows)
        self.assertEqual((s["top1"], s["top3"], s["r5"], s["misses"]), (1, 2, 3, 1))
        self.assertEqual(s["mrr"], round((1 + 1 / 3 + 1 / 5 + 1 / 6) / 5, 3))
        ndcgs = [1.0, 3 / math.log2(4) / 3, 3 / math.log2(6) / 3, 3 / math.log2(7) / 3, 0.0]
        self.assertEqual(s["ndcg"], round(sum(ndcgs) / 5, 3))
        self.assertEqual(s["median_bytes"], 100)


class Cases(unittest.TestCase):
    def test_superseded_wins_only_without_an_excluded_memory(self):
        self.assertTrue(run.won(row(1, case="superseded", excluded=["z"])))
        self.assertFalse(run.won(row(1, case="superseded", excluded=["c"])))

    def test_exception_wins_only_within_the_default_page(self):
        found = [str(n) for n in range(run.SEARCH_PAGE + 1)]
        on_page = dict(row(None), case="exception", found=found, rank=run.SEARCH_PAGE)
        past = dict(on_page, rank=run.SEARCH_PAGE + 1)
        self.assertTrue(run.won(on_page))
        self.assertFalse(run.won(past))
        self.assertFalse(run.won(row(None, case="exception")))

    def test_the_default_page_is_read_from_the_source(self):
        self.assertEqual(run.SEARCH_PAGE, run.rust_usize("src/memory/read.rs", "SEARCH_PAGE"))
        self.assertGreater(run.SEARCH_PAGE, run.NDCG_DEPTH)

    def test_near_duplicate_wins_above_its_rival_or_without_it(self):
        self.assertTrue(run.won(row(2, case="near-duplicate", rival="c")))
        self.assertFalse(run.won(row(2, case="near-duplicate", rival="a")))
        self.assertTrue(run.won(row(2, case="near-duplicate", rival="missing")))
        self.assertFalse(run.won(row(None, case="near-duplicate", rival="missing")))

    def test_cases_count_wins_per_case(self):
        rows = [row(1, case="superseded", excluded=["z"]), row(1, case="superseded", excluded=["c"]),
                row(2, case="near-duplicate", rival="a"), row(1)]
        self.assertEqual(run.cases(rows), {"superseded": {"queries": 2, "wins": 1},
                                           "near-duplicate": {"queries": 1, "wins": 0}})


class Supersedes(unittest.TestCase):
    def test_a_replacement_names_the_stored_id_of_what_it_supersedes(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = run.Store(BINARY, Path(tmp))
            store.stored["old"] = "00000000-0000-4000-8000-000000000001"
            args = store.record_args({"id": "new", "type": "preference", "category": "workflow",
                                      "title": "T", "content": "C", "supersedes": ["old"]})
        at = args.index("--supersedes")
        self.assertEqual(args[at + 1], "00000000-0000-4000-8000-000000000001")

    def test_a_memory_superseding_a_later_one_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            memories = Path(tmp) / "memories.json"
            memories.write_text(json.dumps({"memories": [
                {"id": "new", "type": "preference", "category": "workflow", "title": "T",
                 "content": "C", "supersedes": ["old"]},
                {"id": "old", "type": "preference", "category": "workflow", "title": "U",
                 "content": "D"}]}))
            with self.assertRaises(SystemExit) as stopped:
                run.load(memories)
        self.assertIn("supersedes old, which is not before it", str(stopped.exception))

    def test_every_fixture_replacement_follows_what_it_replaces(self):
        # The fixtures load, so each supersedes names an earlier memory.
        memories, _ = run.load()
        self.assertTrue(any(m.get("supersedes") for m in memories))


if __name__ == "__main__":
    unittest.main()
