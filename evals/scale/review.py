#!/usr/bin/env python3
"""Review speed on large stores, against the budgets of issues #236 and #245.

Builds throwaway stores of synthetic memories (10,000 and 50,000 unless
told otherwise) as the search budget does, makes one in a hundred a light
rewording of another so review has duplicates to find, and times `review`
as one call to a long-running `user-memory serve`, and as a command-line
process for reference: keyword-only, then with a current vector of each
length given for every memory, a hundredth of them copies of another's.
With vectors it times three cases: every pair compared (the kept pairs
forgotten first), the kept pairs with nothing changed, and the kept pairs
after `SAVED` vectors were saved again.
Prints each median and exits with status 1 if one is over its budget.
Nothing here touches the user's own store or settings.

    just review-budget
    just review-budget --memories 20000
    just review-budget --dimensions 1024
"""

import argparse
import importlib.util
import json
import os
import random
import sqlite3
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

_spec = importlib.util.spec_from_file_location("scale_run", Path(__file__).with_name("run.py"))
scale = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scale)

# Store sizes timed: a large store, and five times that.
MEMORIES = [10_000, 50_000]
# Vector lengths timed: a common smaller model's, and qwen3-embedding's
# default, the longest a default install stores.
DIMENSIONS = [1024, 2560]

# Budgets for one review call on the server, keyword-only, by store size, in
# seconds (#236): review is asked for now and then, not every turn, so a
# second at 10,000 memories, and five at 50,000.
KEYWORD_BUDGET_S = {10_000: 1.0, 50_000: 5.0}
# Budgets with vectors, by (store size, vector length), in seconds: the
# median on GitHub's macOS runner (3 cpus) times 1.5, rounded up, so a slower
# run passes and a change that makes review half again slower fails. Every
# pair of vectors is compared exactly, so the time grows with the store's
# size squared times the vector length (#236).
VECTOR_BUDGET_S = {
    (10_000, 1024): 2.2,
    (10_000, 2560): 4.3,
    (50_000, 1024): 53.0,
    (50_000, 2560): 116.0,
}
# Budgets with vectors for a review from kept pairs (#245), by (store size,
# vector length), in seconds: with nothing changed since the last review,
# and after `SAVED` vectors were saved. The macOS runner's median times 1.5,
# rounded up, as above. Most of a review with nothing changed is reading the
# memories and their vectors; the saved vectors add each one compared
# against every other.
KEPT_BUDGET_S = {
    (10_000, 1024): 0.4,
    (10_000, 2560): 0.4,
    (50_000, 1024): 4.3,
    (50_000, 2560): 4.4,
}
SAVED_BUDGET_S = {
    (10_000, 1024): 0.5,
    (10_000, 2560): 0.7,
    (50_000, 1024): 5.5,
    (50_000, 2560): 9.8,
}
# Vectors saved again before each review of the "saved" case: a busy day's
# records and updates on a large store.
SAVED = 100
# The cases timed with vectors, each with its budgets, in the order run:
# the kept pairs are forgotten before each review of the first.
CASES = {"every pair": VECTOR_BUDGET_S, "kept": KEPT_BUDGET_S, "saved": SAVED_BUDGET_S}
CASE_NAMES = {"every pair": "every pair compared", "kept": "kept pairs, nothing changed",
              "saved": f"kept pairs, {SAVED} vectors saved"}

# Timed calls per case; the first of each is a warm-up, not counted.
RUNS = 3

# Memories in a hundred made a light rewording of an earlier one, and given a
# copy of its vector.
COPY_SHARE = 0.01
# The model name the vectors carry, and the cutoffs stored for it: those of
# a calibrated model.
MODEL = "review-budget"
DUPLICATE_CUTOFF = 0.9
RELATED_CUTOFF = 0.8
# Distinct vectors the memories' vectors are rolled from: each memory gets
# one rolled by its own offset, so no two memories share a vector unless
# copied, and unrelated ones are close to orthogonal.
VECTOR_POOL = 1_000
# Fixed, so every run times the same stores.
SEED = 23


def plant_copies(db):
    """Makes `COPY_SHARE` of the memories a copy of an earlier one with its
    last word dropped; returns (copy rowid, original rowid) pairs."""
    rng = random.Random(SEED)
    conn = sqlite3.connect(db)
    rows = conn.execute("SELECT rowid, content, category FROM memories ORDER BY rowid").fetchall()
    pairs = []
    for index in range(1, len(rows)):
        if rng.random() < COPY_SHARE:
            original = rows[rng.randrange(index)]
            conn.execute("UPDATE memories SET content=?, category=? WHERE rowid=?",
                         (original[1].rsplit(" ", 1)[0], original[2], rows[index][0]))
            pairs.append((rows[index][0], original[0]))
    conn.commit()
    conn.close()
    return pairs


def add_vectors(db, dimensions, copies):
    """Gives every memory a current vector (see `scale.add_vectors`), each
    unique but those of `copies`, and stores the model's cutoffs."""
    pool = [struct.pack(f"<{dimensions}f", *scale.unit_vector(i, dimensions)) for i in range(VECTOR_POOL)]
    copied = dict(copies)
    conn = sqlite3.connect(db)
    conn.execute("DELETE FROM memory_vectors")
    conn.execute("INSERT OR REPLACE INTO embedding_cutoffs(model,length,duplicate,related) VALUES (?,?,?,?)",
                 (MODEL, dimensions, DUPLICATE_CUTOFF, RELATED_CUTOFF))
    rows = conn.execute("SELECT rowid,id,category,memory_type,content FROM memories ORDER BY rowid").fetchall()

    def vector(rowid):
        while rowid in copied:
            rowid = copied[rowid]
        packed = pool[rowid % VECTOR_POOL]
        offset = 4 * ((rowid // VECTOR_POOL) % dimensions)
        return packed[offset:] + packed[:offset]

    for start in range(0, len(rows), scale.BATCH):
        conn.executemany(
            "INSERT INTO memory_vectors(id,model,dimensions,source,vector) VALUES (?,?,?,?,?)",
            [(id, MODEL, dimensions, f"{category} ({kind}): {content}", vector(rowid))
             for rowid, id, category, kind, content in rows[start:start + scale.BATCH]])
        conn.commit()
    conn.close()


def ready(db, case):
    """Sets the store up for one timed review of `case` (see `CASES`): the
    kept pairs forgotten, so it compares every pair; nothing changed; or
    `SAVED` vectors saved again."""
    conn = sqlite3.connect(db)
    if case == "every pair":
        conn.execute("DELETE FROM memory_pairs_state")
    elif case == "saved":
        conn.execute("UPDATE memory_vectors SET source=source WHERE rowid IN "
                     "(SELECT rowid FROM memory_vectors ORDER BY random() LIMIT ?)", (SAVED,))
    conn.commit()
    conn.close()


def median_s(samples):
    return round(statistics.median(samples[1:]), 2)


def time_cli(binary, env, before=lambda: None):
    samples = []
    for _ in range(RUNS + 1):
        before()
        started = time.perf_counter()
        subprocess.run([str(binary), "review", "--max-groups", "20"], env=env, check=True,
                       capture_output=True)
        samples.append(time.perf_counter() - started)
    return median_s(samples)


def time_server(binary, env, before=lambda: None):
    """The median review call's time on one `serve`, and its group count;
    `before` runs ahead of each call, untimed."""
    server = scale.Server(binary, env)
    try:
        samples = []
        for _ in range(RUNS + 1):
            before()
            started = time.perf_counter()
            reply = server.call("tools/call", {"name": "review", "arguments": {"max_groups": 20}})
            samples.append(time.perf_counter() - started)
            if reply["result"].get("isError"):
                raise RuntimeError(reply["result"]["content"][0]["text"])
        groups = len(json.loads(reply["result"]["content"][0]["text"])["groups"])
        return median_s(samples), groups
    finally:
        server.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/user-memory")
    parser.add_argument("--memories", type=int, nargs="+", default=MEMORIES)
    parser.add_argument("--dimensions", type=int, nargs="+", default=DIMENSIONS)
    args = parser.parse_args()
    over = []
    print(f"{os.cpu_count()} cpus; median of {RUNS} after a warm-up")
    for count in args.memories:
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            home = tmp / "home"
            home.mkdir()
            db = tmp / "memories.sqlite3"
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
                   "XDG_DATA_HOME": str(home / ".local/share"), "XDG_STATE_HOME": str(home / ".local/state"),
                   "XDG_CONFIG_HOME": str(home / ".config"), "USER_MEMORY_DB": str(db),
                   "USER_MEMORY_NO_UPDATE_CHECK": "1", **{name: "" for name in scale.EMBED_SETTINGS}}
            scale.build(db, args.binary, env, count)
            copies = plant_copies(db)
            served, groups = time_server(args.binary, env)
            cli = time_cli(args.binary, env)
            print(f"{count:,} memories, keyword-only: server {served} s, command line {cli} s, {groups} groups",
                  flush=True)
            budget = KEYWORD_BUDGET_S.get(count)
            if budget is not None and served > budget:
                over.append(f"{count:,} keyword-only: {served} s > {budget} s")
            for dimensions in args.dimensions:
                add_vectors(db, dimensions, copies)
                semantic = {**env, "USER_MEMORY_EMBED_URL": "http://127.0.0.1:9/v1/embeddings",
                            "USER_MEMORY_EMBED_ALLOW_HTTP": "1", "USER_MEMORY_EMBED_MODEL": MODEL,
                            "USER_MEMORY_EMBED_DIMENSIONS": str(dimensions)}
                for case, budgets in CASES.items():
                    before = lambda case=case: ready(db, case)
                    served, groups = time_server(args.binary, semantic, before)
                    cli = time_cli(args.binary, semantic, before)
                    print(f"{count:,} memories, {dimensions} dimensions, {CASE_NAMES[case]}: server {served} s, "
                          f"command line {cli} s, {groups} groups", flush=True)
                    budget = budgets.get((count, dimensions))
                    if budget is not None and served > budget:
                        over.append(f"{count:,} at {dimensions} dimensions, {CASE_NAMES[case]}: "
                                    f"{served} s > {budget} s")
    if over:
        print("Over budget:\n  " + "\n  ".join(over))
        sys.exit(1)


if __name__ == "__main__":
    main()
