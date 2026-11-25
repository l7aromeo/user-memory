#!/usr/bin/env python3
"""Checks of the retrieval benchmark: its metrics on hand-computed ranks,
its cases, the fixtures' `supersedes` field, and its private mode (#78):
an export loaded as it is, paths inside the repository and endpoints off
this machine refused, and no text of a private set in what it prints or
writes. Run with `python3 -I evals/retrieval/test_run.py`; the checks that
run the binary need `cargo build` first and say so when it is missing."""

import importlib.util
import json
import math
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
                run.load(run.parse(["--memories", str(memories)]))
        self.assertIn("supersedes old, which is not before it", str(stopped.exception))

    def test_every_fixture_replacement_follows_what_it_replaces(self):
        # The fixtures load, so each supersedes names an earlier memory.
        memories, export, _, private = run.load(run.parse([]))
        self.assertIsNone(export)
        self.assertFalse(private)
        self.assertTrue(any(m.get("supersedes") for m in memories))


class Paths(unittest.TestCase):
    def test_each_path_inside_the_repository_is_refused(self):
        inside = str(run.ROOT / "evals" / "retrieval" / "private.json")
        for name in ("memories", "queries", "out", "keep"):
            with self.subTest(name=name):
                self.assertEqual(run.refused_path(run.parse([f"--{name}", inside])), name)

    def test_a_path_reaching_into_the_repository_through_a_link_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            link = Path(tmp) / "repo"
            link.symlink_to(run.ROOT)
            self.assertEqual(run.refused_path(run.parse(["--out", str(link / "out.json")])), "out")

    def test_paths_outside_it_are_kept(self):
        with tempfile.TemporaryDirectory() as tmp:
            outside = str(Path(tmp) / "x.json")
            args = run.parse(["--memories", outside, "--queries", outside, "--out", outside,
                              "--keep", tmp])
            self.assertIsNone(run.refused_path(args))


class Endpoint(unittest.TestCase):
    def test_loopback_hosts_are_on_this_machine(self):
        for url in ("http://localhost:11434/v1/embeddings", "http://127.0.0.2:11434/v1",
                    "http://[::1]:11434/v1"):
            with self.subTest(url=url):
                self.assertTrue(run.on_this_machine(url))

    def test_a_name_resolving_off_this_machine_too_is_not(self):
        both = [(None, None, None, "", ("127.0.0.1", 0)), (None, None, None, "", ("203.0.113.5", 0))]
        with mock.patch.object(run.socket, "getaddrinfo", return_value=both):
            self.assertFalse(run.on_this_machine("http://embeddings.lan:11434/v1"))
        with mock.patch.object(run.socket, "getaddrinfo", return_value=both[:1]):
            self.assertTrue(run.on_this_machine("http://embeddings.lan:11434/v1"))

    def test_other_hosts_are_not(self):
        # 203.0.113.0/24 is reserved for documentation; .invalid never resolves.
        for url in ("http://203.0.113.5:11434/v1", "https://api.example.invalid/v1", "", "not a url"):
            with self.subTest(url=url):
                self.assertFalse(run.on_this_machine(url))


# A private set's text, which nothing the benchmark prints or writes may hold.
PRIVATE_TITLE = "Quillfeather invoices go out on the third"
PRIVATE_CONTENT = "Send the Quillfeather invoices on the third working day of each month."
PRIVATE_QUERY = "when do quillfeather invoices go out"


@unittest.skipUnless(BINARY.exists(), f"{BINARY} is not built; run cargo build")
class Private(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.memories = self.tmp / "memories.json"
        self.memories.write_text(json.dumps({"memories": [
            {"id": "invoices", "type": "preference", "category": "workflow",
             "title": PRIVATE_TITLE, "content": PRIVATE_CONTENT}]}))
        self.queries = self.tmp / "queries.json"
        self.out = self.tmp / "out.json"

    def bench(self, *extra, target="invoices"):
        self.queries.write_text(json.dumps({"project": "github.com/example/private", "queries": [
            {"style": "direct", "query": PRIVATE_QUERY, "target": target}]}))
        return subprocess.run(
            [sys.executable, "-I", str(HERE / "run.py"), "--binary", str(BINARY),
             "--memories", str(self.memories), "--queries", str(self.queries),
             "--out", str(self.out), *extra], capture_output=True, text=True, timeout=300)

    def assert_no_text(self, *texts):
        for text in texts:
            for private in (PRIVATE_TITLE, PRIVATE_CONTENT, PRIVATE_QUERY, "Quillfeather",
                            "quillfeather"):
                self.assertNotIn(private, text)

    def test_the_report_and_out_file_hold_no_text_of_the_set(self):
        done = self.bench()
        self.assertEqual(done.returncode, 0, done.stderr)
        written = self.out.read_text()
        self.assert_no_text(done.stdout, done.stderr, written)
        rows = json.loads(written)["keyword"]["rows"]
        self.assertEqual((rows[0]["number"], rows[0]["rank"]), (1, 1))

    def test_an_error_names_the_query_by_number(self):
        done = self.bench(target="nowhere")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("query 1 names unknown or ambiguous memory nowhere", done.stderr)
        self.assert_no_text(done.stdout, done.stderr)
        self.assertFalse(self.out.exists())

    def test_an_endpoint_off_this_machine_is_refused_before_anything_is_sent(self):
        done = self.bench("--embed-url", "http://203.0.113.5:9/v1/embeddings")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("refusing: the embeddings endpoint 203.0.113.5 is not on this machine",
                      done.stderr)
        self.assertFalse(self.out.exists())

    def test_a_path_inside_the_repository_is_refused(self):
        inside = run.ROOT / "evals" / "retrieval" / "out.json"
        done = self.bench("--out", str(inside))
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("is inside the repository", done.stderr)
        self.assertFalse(inside.exists())


def export_of(store, path):
    """The memories of `store`'s export at `path`, by id, with the fields an
    import must keep."""
    store.run(["export", str(path), "--force"], {})
    kept = ("status", "source", "evidence", "evidence_ids", "superseded_by", "scope", "project",
            "title", "content")
    return {m["id"]: {k: m.get(k) for k in kept}
            for m in json.loads(path.read_text())["memories"]}


@unittest.skipUnless(BINARY.exists(), f"{BINARY} is not built; run cargo build")
class Export(unittest.TestCase):
    def test_an_export_loads_with_its_ids_statuses_evidence_and_supersessions(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "a").mkdir()
            (tmp / "b").mkdir()
            source = run.Store(BINARY, tmp / "a")

            def record(*args):
                reply = json.loads(source.run(["record", *args, "--scope", "global"], {}))
                self.assertTrue(reply.get("stored"), reply)
                return reply["id"]
            old = record("--type", "preference", "--category", "workflow",
                         "--title", "Hold the review at nine", "--content", "Reviews start at nine.")
            record("--type", "preference", "--category", "workflow",
                   "--title", "Hold the review at ten", "--content", "Reviews start at ten.",
                   "--supersedes", old)
            seen = [record("--type", "preference", "--category", "tooling", "--source", "observed",
                           "--title", f"Uses {tool} for {use}", "--content", f"Reaches for {tool}.",
                           "--evidence", f"Ran {tool} in the session of 2026-11-0{n}.")
                    for n, (tool, use) in enumerate([("ripgrep", "search"), ("jq", "JSON")], 1)]
            record("--type", "preference", "--category", "tooling", "--source", "inferred",
                   "--title", "Prefers small command-line tools", "--content",
                   "Picks a small single-purpose tool over a larger one.",
                   "--evidence-id", seen[0], "--evidence-id", seen[1])
            before = export_of(source, tmp / "before.json")
            statuses = sorted(m["status"] for m in before.values())
            self.assertIn("superseded", statuses)
            self.assertEqual({m["source"] for m in before.values()},
                             {"explicit", "observed", "inferred"})

            target = run.Store(BINARY, tmp / "b")
            target.load_export(tmp / "before.json")
            self.assertEqual(export_of(target, tmp / "after.json"), before)

    def test_an_export_is_told_from_the_benchmark_format(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "export.json"
            path.write_text(json.dumps({"format": run.EXPORT_FORMAT, "memories": [
                {"id": "e0000001-0000-4000-8000-000000000001", "scope": "global"}]}))
            queries = Path(tmp) / "queries.json"
            queries.write_text(json.dumps({"project": "p", "queries": [
                {"style": "direct", "query": "q", "target": "e0000001"}]}))
            _, export, spec_, private = run.load(
                run.parse(["--memories", str(path), "--queries", str(queries)]))
        self.assertEqual(export, path)
        self.assertTrue(private)
        self.assertEqual(spec_["queries"][0]["target"], "e0000001-0000-4000-8000-000000000001")


if __name__ == "__main__":
    unittest.main()
