#!/usr/bin/env python3
"""Retrieval benchmark: how well `user-memory search` finds a known memory.

Builds a throwaway store from memories.json, runs every query in
queries.json through `user-memory search` as the session's project would,
and reports top1, top3, mean reciprocal rank (MRR) and misses per query
style, keyword-only and, with an embeddings endpoint, fused with semantic
similarity. Nothing here touches the user's own store or settings: the
store, home and state live in a temporary folder.

    just retrieval-bench
    just retrieval-bench --embed-url http://127.0.0.1:11434/v1/embeddings
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

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


class Store:
    """A store of its own in `root`, reached through the binary."""

    def __init__(self, binary, root):
        self.binary = binary
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
        self.ids = {}

    def env(self, embed):
        env = dict(self.base)
        env.update({name: "" for name in EMBED_SETTINGS})
        env.update(embed)
        return env

    def run(self, args, embed):
        out = subprocess.run([str(self.binary), *args], env=self.env(embed),
                             capture_output=True, text=True, timeout=120)
        if out.returncode != 0:
            raise RuntimeError(f"user-memory {args[0]} failed: {out.stderr.strip()}")
        return out.stdout

    def record(self, memory):
        args = ["record", "--type", memory["type"], "--category", memory["category"],
                "--title", memory["title"], "--content", memory["content"],
                "--scope", memory.get("scope", "global"), "--project", memory.get("project", "")]
        reply = json.loads(self.run(args, {}))
        if not reply.get("stored") and reply.get("similar"):
            # The benchmark's memories are distinct by design.
            for similar in reply["similar"]:
                args += ["--distinct-from", similar["id"]]
            reply = json.loads(self.run(args, {}))
        if not reply.get("stored"):
            raise RuntimeError(f"{memory['id']} was not stored: {reply}")
        self.ids[reply["id"]] = memory["id"]

    def search(self, query, project, embed):
        reply = json.loads(self.run(
            ["search", query, "--project", project, "--page-size", PAGE], embed))
        return [self.ids.get(m["id"], m["id"]) for m in reply["memories"]], reply.get("semantic")


def score(rows):
    """top1, top3, MRR and misses over `rows`, each with its target's rank."""
    ranks = [row["rank"] for row in rows]
    return {
        "queries": len(rows),
        "top1": sum(1 for r in ranks if r == 1),
        "top3": sum(1 for r in ranks if r is not None and r <= 3),
        "mrr": round(sum(1 / r for r in ranks if r) / len(rows), 3),
        "misses": sum(1 for r in ranks if r is None),
        "median_matched": statistics.median(row["matched"] for row in rows),
    }


def table(name, rows):
    styles = sorted({row["style"] for row in rows})
    print(f"\n{name}")
    print(f"  {'style':<11}{'n':>4}{'top1':>6}{'top3':>6}{'MRR':>7}{'miss':>6}{'matched':>9}")
    for style in [*styles, "all"]:
        part = rows if style == "all" else [r for r in rows if r["style"] == style]
        s = score(part)
        print(f"  {style:<11}{s['queries']:>4}{s['top1']:>6}{s['top3']:>6}{s['mrr']:>7.3f}"
              f"{s['misses']:>6}{s['median_matched']:>9}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/user-memory")
    parser.add_argument("--embed-url", default="",
                        help="an OpenAI-compatible embeddings endpoint; without it, keyword-only")
    parser.add_argument("--embed-model", default=QWEN_MODEL)
    parser.add_argument("--embed-dimensions", default=QWEN_DIMENSIONS)
    parser.add_argument("--query-instruction", default=QWEN_INSTRUCTION)
    parser.add_argument("--out", type=Path, help="write every query's ranks here as JSON")
    parser.add_argument("--keep", type=Path,
                        help="copy the benchmark's store (with its vectors) here when done")
    args = parser.parse_args()

    memories = json.loads((HERE / "memories.json").read_text())["memories"]
    spec = json.loads((HERE / "queries.json").read_text())
    known = {m["id"] for m in memories}
    for q in spec["queries"]:
        for wanted in [q["target"], *q.get("related", [])]:
            if wanted not in known:
                sys.exit(f"query {q['query']!r} names unknown memory {wanted}")
    outside = {m["id"] for m in memories if m.get("project", spec["project"]) != spec["project"]}
    project_ids = {m["id"] for m in memories if m.get("project") == spec["project"]}

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
        store = Store(args.binary, Path(tmp))
        for memory in memories:
            store.record(memory)
        if "fused" in modes:
            store.run(["reindex"], modes["fused"])
        results = {}
        for mode, embed in modes.items():
            rows = []
            for q in spec["queries"]:
                found, semantic = store.search(q["query"], spec["project"], embed)
                if mode == "fused" and not semantic:
                    sys.exit(f"the endpoint did not rank {q['query']!r}; is it running?")
                leaked = outside.intersection(found)
                if leaked:
                    sys.exit(f"{q['query']!r} returned another project's memories: {sorted(leaked)}")
                rank = found.index(q["target"]) + 1 if q["target"] in found else None
                rows.append({"style": q["style"], "query": q["query"], "target": q["target"],
                             "rank": rank, "matched": len(found), "found": found})
            results[mode] = {"summary": score(rows), "rows": rows}
            table(mode, rows)
            # Relevance alone: global targets ranked among global memories,
            # with the project's memories, which come first, left out.
            ranked = []
            for row in rows:
                if row["target"] in project_ids:
                    continue
                among = [m for m in row["found"] if m not in project_ids]
                rank = among.index(row["target"]) + 1 if row["target"] in among else None
                ranked.append({**row, "rank": rank, "matched": len(among)})
            results[mode]["global_only"] = score(ranked)
            table(f"{mode}, global targets among global memories", ranked)
        if args.keep:
            args.keep.mkdir(parents=True, exist_ok=True)
            shutil.copy(Path(tmp) / "memories.sqlite3", args.keep / "memories.sqlite3")
            (args.keep / "ids.json").write_text(json.dumps(store.ids, indent=1) + "\n")
    if args.out:
        args.out.write_text(json.dumps(results, indent=1) + "\n")


main()
