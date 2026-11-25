#!/usr/bin/env python3
"""Retrieval benchmark: how well `user-memory search` finds a known memory.

Builds a throwaway store from memories.json, runs every query in
queries.json through `user-memory search` as the session's project would,
and reports top1, top3, recall at 5 (R@5), mean reciprocal rank (MRR),
NDCG at 10 over each query's grades, misses and the default page's size
per query style, keyword-only and, with an embeddings endpoint, fused with
semantic similarity. A query that names the part of a long memory it asks
about (`passage`) also counts whether its target came back with that part
as its passage. Queries of a `case` count their wins apart: a superseded
memory never returned, a project's exception to a global rule within the
default page, and a memory ranked above a near-duplicate that means
something else.

`--memories` and `--queries` replace the fixtures with files of your own,
such as the export of a private copy of your store. Then nothing it prints
or writes holds a query's or a memory's text, only numbers and ids, and an
embeddings endpoint must be on this machine. Nothing here touches the
user's own store or settings: the store, home and state live in a
temporary folder.

    just retrieval-bench
    just retrieval-bench --embed-url http://127.0.0.1:11434/v1/embeddings
"""

import argparse
import ipaddress
import json
import math
import os
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent

# The installer's settings for qwen3-embedding through Ollama, which the
# endpoint options default to.
QWEN_MODEL = "qwen3-embedding:4b"
QWEN_DIMENSIONS = "1024"
QWEN_INSTRUCTION = "Instruct: Given a question, retrieve the user's matching memory\\nQuery:"

# Every embedding setting the binary reads; each run sets all of them, so the
# caller's own environment never leaks in.
EMBED_SETTINGS = (
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
)

# Enough to see every match the search returns, past the default page.
PAGE = "200"

# The `format` an export file names (src/memory/transfer.rs), by which
# `--memories` tells one from the benchmark's own format.
EXPORT_FORMAT = "user-memory-export"
# The characters of a memory id the index shows, which a query file aimed
# at an export may use instead of the whole id.
SHORT_ID = 8
# The rank recall counts the target within: the first screen of results.
RECALL_DEPTH = 5
# The ranks NDCG counts: half the default page, what an agent reads first.
NDCG_DEPTH = 10
# A query's grades for NDCG: its target, and the memories it names as
# related (queries.json's note).
TARGET_GRADE = 2
RELATED_GRADE = 1
# The cases a query may test, beside its wording's style.
CASES = ("superseded", "exception", "near-duplicate")


def rust_usize(path, name):
    """The value of the `usize` constant `name` in `path`."""
    line = next(l for l in (ROOT / path).read_text().splitlines()
                if f"const {name}: usize = " in l)
    return int(line.split("= ")[1].rstrip(";").replace("_", ""))


# The search's page when an agent asks for no other size: an exception the
# project holds must be on it.
SEARCH_PAGE = rust_usize("src/memory/read.rs", "SEARCH_PAGE")


class Store:
    """A store of its own in `root`, reached through the binary. A private
    store's errors leave out what the binary said, which may quote it."""

    def __init__(self, binary, root, private=False):
        self.binary = binary
        self.private = private
        home = root / "home"
        home.mkdir()
        self.base = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(home),
            "XDG_DATA_HOME": str(home / ".local/share"),
            "XDG_STATE_HOME": str(home / ".local/state"),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "USER_MEMORY_DB": str(root / "memories.sqlite3"),
            "USER_MEMORY_NO_UPDATE_CHECK": "1",
        }
        # Each stored id to the name the benchmark gives it, and back. An
        # export keeps its ids, so neither holds its memories.
        self.ids = {}
        self.stored = {}

    def env(self, embed):
        env = dict(self.base)
        env.update({name: "" for name in EMBED_SETTINGS})
        env.update(embed)
        return env

    def run(self, args, embed):
        out = subprocess.run([str(self.binary), *args], env=self.env(embed),
                             capture_output=True, text=True, timeout=120)
        if out.returncode != 0:
            said = f"exit {out.returncode}" if self.private else out.stderr.strip()
            raise RuntimeError(f"user-memory {args[0]} failed: {said}")
        return out.stdout

    def record_args(self, memory):
        """The `record` command for one of the benchmark's memories, each
        memory it supersedes named by its stored id."""
        args = ["record", "--type", memory["type"], "--category", memory["category"],
                "--title", memory["title"], "--content", memory["content"],
                "--scope", memory.get("scope", "global"), "--project", memory.get("project", "")]
        for old in memory.get("supersedes", []):
            args += ["--supersedes", self.stored[old]]
        return args

    def record(self, memory):
        args = self.record_args(memory)
        reply = json.loads(self.run(args, {}))
        if not reply.get("stored") and reply.get("similar"):
            # The benchmark's memories are distinct by design.
            for similar in reply["similar"]:
                args += ["--distinct-from", similar["id"]]
            reply = json.loads(self.run(args, {}))
        if not reply.get("stored"):
            said = "" if self.private else f": {reply}"
            raise RuntimeError(f"{memory['id']} was not stored{said}")
        self.ids[reply["id"]] = memory["id"]
        self.stored[memory["id"]] = reply["id"]

    def load_export(self, path):
        """Imports the export at `path`, which keeps its ids, statuses,
        evidence, supports and supersessions as they are."""
        self.run(["import", str(path), "--apply"], {})

    def search(self, query, project, embed):
        """The ids `query` finds in `project`, best first, whether it was
        ranked semantically, each found memory's passage, and the bytes of
        the default page as compact JSON, as the MCP server sends it."""
        reply = json.loads(self.run(
            ["search", query, "--project", project, "--page-size", PAGE], embed))
        found = [self.ids.get(m["id"], m["id"]) for m in reply["memories"]]
        passages = {self.ids.get(m["id"], m["id"]): m.get("passage") for m in reply["memories"]}
        page = json.loads(self.run(["search", query, "--project", project], embed))
        size = len(json.dumps(page, separators=(",", ":")).encode())
        return found, reply.get("semantic"), passages, size


def rank_of(found, memory):
    """`memory`'s rank in `found`, from 1, or None when it is missing."""
    return found.index(memory) + 1 if memory in found else None


def grades(query):
    """Each memory `query` grades, with its grade."""
    graded = {m: RELATED_GRADE for m in query.get("related", [])}
    graded[query["target"]] = TARGET_GRADE
    return graded


def ndcg(found, graded, depth=NDCG_DEPTH):
    """Normalised discounted cumulative gain of `found`'s first `depth`
    ranks: each graded memory gains 2^grade - 1, divided by log2(rank + 1),
    over the same sum for the best order of the graded memories."""
    def gain(order):
        return sum((2 ** g - 1) / math.log2(rank + 1)
                   for rank, g in enumerate(order[:depth], 1))
    best = gain(sorted(graded.values(), reverse=True))
    return gain([graded.get(m, 0) for m in found]) / best if best else 0.0


def won(row):
    """Whether `row`'s query met its case: no superseded memory returned,
    the project's exception within the default page, or the target above
    the near-duplicate it is not (a rival not returned at all included)."""
    case, found = row["case"], row["found"]
    if case == "superseded":
        return not set(row["excluded"]).intersection(found)
    if case == "exception":
        return row["rank"] is not None and row["rank"] <= SEARCH_PAGE
    if case == "near-duplicate":
        rival = rank_of(found, row["rival"])
        return row["rank"] is not None and (rival is None or row["rank"] < rival)
    raise ValueError(f"unknown case {case!r}")


def score(rows):
    """top1, top3, R@5, MRR, NDCG, misses, the median matched and page
    bytes, and passages over `rows`, each with its target's rank and NDCG."""
    ranks = [row["rank"] for row in rows]
    return {
        "queries": len(rows),
        "top1": sum(1 for r in ranks if r == 1),
        "top3": sum(1 for r in ranks if r is not None and r <= 3),
        "r5": sum(1 for r in ranks if r is not None and r <= RECALL_DEPTH),
        "mrr": round(sum(1 / r for r in ranks if r) / len(rows), 3),
        "ndcg": round(statistics.mean(row["ndcg"] for row in rows), 3),
        "misses": sum(1 for r in ranks if r is None),
        "median_matched": statistics.median(row["matched"] for row in rows),
        "median_bytes": statistics.median(row["bytes"] for row in rows),
        # Of the queries naming the part of a long memory they ask about,
        # those whose target came back with that part as its passage.
        "passages": sum(1 for row in rows if row.get("passage_found")),
        "passage_queries": sum(1 for row in rows if "passage_found" in row),
    }


def cases(rows):
    """Each case's queries and wins among `rows`."""
    return {case: {"queries": len(part), "wins": sum(1 for row in part if won(row))}
            for case in CASES
            if (part := [row for row in rows if row.get("case") == case])}


def table(name, rows):
    styles = sorted({row["style"] for row in rows})
    print(f"\n{name}")
    print(f"  {'style':<11}{'n':>4}{'top1':>6}{'top3':>6}{'R@5':>5}{'MRR':>7}{'NDCG':>7}"
          f"{'miss':>6}{'matched':>9}{'bytes':>8}")
    for style in [*styles, "all"]:
        part = rows if style == "all" else [r for r in rows if r["style"] == style]
        s = score(part)
        print(f"  {style:<11}{s['queries']:>4}{s['top1']:>6}{s['top3']:>6}{s['r5']:>5}"
              f"{s['mrr']:>7.3f}{s['ndcg']:>7.3f}{s['misses']:>6}{s['median_matched']:>9}"
              f"{s['median_bytes']:>8.0f}")
    s = score(rows)
    if s["passage_queries"]:
        print(f"  passages: {s['passages']} of {s['passage_queries']} queries about part of a long "
              "memory got that part as the passage")
    for case, c in cases(rows).items():
        print(f"  {case}: {c['wins']} of {c['queries']} queries won")


def parse(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/user-memory")
    parser.add_argument("--embed-url", default="",
                        help="an OpenAI-compatible embeddings endpoint; without it, keyword-only")
    parser.add_argument("--embed-model", default=QWEN_MODEL)
    parser.add_argument("--embed-dimensions", default=QWEN_DIMENSIONS)
    parser.add_argument("--query-instruction", default=QWEN_INSTRUCTION)
    parser.add_argument("--memories", type=Path,
                        help="memories in the benchmark's format, or a `user-memory export` "
                             "file, instead of memories.json")
    parser.add_argument("--queries", type=Path,
                        help="queries in the benchmark's format instead of queries.json")
    parser.add_argument("--out", type=Path, help="write every query's ranks here as JSON")
    parser.add_argument("--keep", type=Path,
                        help="copy the benchmark's store (with its vectors) here when done")
    return parser.parse_args(argv)


def refused_path(args):
    """The first option naming a path inside the repository, which holds no
    private memories nor what is made from them, or None."""
    for name in ("memories", "queries", "out", "keep"):
        path = getattr(args, name)
        if path is not None and path.resolve().is_relative_to(ROOT):
            return name
    return None


def on_this_machine(url):
    """Whether every address `url`'s host resolves to is a loopback
    address, so what is sent to it stays on this machine. A name that does
    not resolve is not."""
    host = urlsplit(url).hostname
    if not host:
        return False
    try:
        addresses = {info[4][0] for info in socket.getaddrinfo(host, None)}
    except (OSError, UnicodeError):
        return False
    return bool(addresses) and all(
        ipaddress.ip_address(address.split("%")[0]).is_loopback for address in addresses)


def resolve(name, known, short):
    """The id in `known` that `name` names: itself, or with `short`, the
    one id it begins, of at least SHORT_ID characters. None otherwise."""
    if name in known:
        return name
    if short and len(name) >= SHORT_ID:
        matches = [k for k in known if k.startswith(name)]
        if len(matches) == 1:
            return matches[0]
    return None


def load(args):
    """The memories (id, scope, project, as the benchmark or an export holds
    them), the export to import if `--memories` is one, and the queries,
    each id checked and resolved. Exits naming a query by number, never by
    its text, when the files are your own."""
    private = args.memories is not None or args.queries is not None
    document = json.loads((args.memories or HERE / "memories.json").read_text())
    export = args.memories if document.get("format") == EXPORT_FORMAT else None
    memories = document["memories"]
    spec = json.loads((args.queries or HERE / "queries.json").read_text())
    before = set()
    for memory in memories:
        for old in memory.get("supersedes", []):
            if export is None and old not in before:
                sys.exit(f"memory {memory['id']} supersedes {old}, which is not before it")
        before.add(memory["id"])
    known = {m["id"] for m in memories}
    for number, q in enumerate(spec["queries"], 1):
        named = f"query {number}" if private else repr(q["query"])
        case = q.get("case")
        if case is not None and case not in CASES:
            sys.exit(f"{named} has unknown case {case!r}")
        if case == "near-duplicate" and "rival" not in q:
            sys.exit(f"{named} is a near-duplicate case without a rival")
        if case == "superseded" and not q.get("excluded"):
            sys.exit(f"{named} is a superseded case without excluded memories")
        for field in ("target", "rival"):
            if field in q:
                q[field] = check(q[field], known, export, named)
        for field in ("related", "excluded"):
            q[field] = [check(m, known, export, named) for m in q.get(field, [])]
    return memories, export, spec, private


def check(name, known, export, named):
    """`name` resolved against `known`, or exit naming the query."""
    found = resolve(name, known, export is not None)
    if found is None:
        sys.exit(f"{named} names unknown or ambiguous memory {name}")
    return found


def main(argv=None):
    args = parse(argv)
    if (name := refused_path(args)) is not None:
        sys.exit(f"refusing: --{name} {getattr(args, name)} is inside the repository; "
                 "private memories and what is made from them stay outside it")
    memories, export, spec, private = load(args)
    if private and args.embed_url and not on_this_machine(args.embed_url):
        sys.exit(f"refusing: the embeddings endpoint {urlsplit(args.embed_url).hostname} is not on "
                 "this machine, and semantic search sends it memory text (docs/privacy.md); with "
                 "--memories or --queries, give an endpoint on a loopback address or none")

    def in_project(m):
        return m.get("scope", "global") == "project"
    outside = {m["id"] for m in memories if in_project(m) and m.get("project") != spec["project"]}
    project_ids = {m["id"] for m in memories if in_project(m) and m.get("project") == spec["project"]}

    modes = {"keyword": {}}
    if args.embed_url:
        modes["fused"] = {
            "USER_MEMORY_EMBED_URL": args.embed_url,
            "USER_MEMORY_EMBED_MODEL": args.embed_model,
            "USER_MEMORY_EMBED_DIMENSIONS": args.embed_dimensions,
            "USER_MEMORY_EMBED_QUERY_INSTRUCTION": args.query_instruction,
            "USER_MEMORY_EMBED_ALLOW_HTTP": "1",
        }

    with tempfile.TemporaryDirectory(prefix="user-memory-retrieval-") as tmp:
        store = Store(args.binary, Path(tmp), private)
        if export is not None:
            store.load_export(export)
        else:
            for memory in memories:
                store.record(memory)
        if "fused" in modes:
            store.run(["reindex"], modes["fused"])
        results = {}
        for mode, embed in modes.items():
            rows = []
            for number, q in enumerate(spec["queries"], 1):
                named = f"query {number}" if private else repr(q["query"])
                found, semantic, passages, size = store.search(q["query"], spec["project"], embed)
                if mode == "fused" and not semantic:
                    sys.exit(f"the endpoint did not rank {named}; is it running?")
                leaked = outside.intersection(found)
                if leaked:
                    sys.exit(f"{named} returned another project's memories: {sorted(leaked)}")
                row = {"style": q["style"], "query": q["query"], "target": q["target"],
                       "rank": rank_of(found, q["target"]), "ndcg": ndcg(found, grades(q)),
                       "matched": len(found), "bytes": size, "found": found}
                if private:
                    row = {"number": number, **row}
                    del row["query"]
                for field in ("case", "rival", "excluded"):
                    if field in q:
                        row[field] = q[field]
                if "passage" in q:
                    shown = passages.get(q["target"]) or ""
                    row["passage_found"] = q["passage"].lower() in shown.lower()
                if "case" in q:
                    row["won"] = won(row)
                row["graded"] = grades(q)
                rows.append(row)
            results[mode] = {"summary": score(rows), "cases": cases(rows), "rows": rows}
            table(mode, rows)
            # Relevance alone: global targets ranked among global memories,
            # with the project's memories, which come first, left out.
            ranked = []
            for row in rows:
                if row["target"] in project_ids:
                    continue
                among = [m for m in row["found"] if m not in project_ids]
                graded = {m: g for m, g in row["graded"].items() if m not in project_ids}
                ranked.append({**row, "rank": rank_of(among, row["target"]),
                               "ndcg": ndcg(among, graded), "matched": len(among)})
            results[mode]["global_only"] = score(ranked)
            table(f"{mode}, global targets among global memories",
                  [{k: v for k, v in r.items() if k != "case"} for r in ranked])
        if args.keep:
            args.keep.mkdir(parents=True, exist_ok=True)
            shutil.copy(Path(tmp) / "memories.sqlite3", args.keep / "memories.sqlite3")
            (args.keep / "ids.json").write_text(json.dumps(store.ids, indent=1) + "\n")
    if args.out:
        args.out.write_text(json.dumps(results, indent=1) + "\n")


if __name__ == "__main__":
    main()
