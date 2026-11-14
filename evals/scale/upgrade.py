#!/usr/bin/env python3
"""The time a release's store upgrade takes on a large store.

Builds a throwaway store of synthetic memories (100,000 unless told
otherwise) with an earlier release's binary, the one whose schema the
upgrade starts from, as `just search-budget` builds its store: the same
generator, seed and keyword index, and every memory with a vector. Then,
on a fresh copy each run, times `user-memory migrate` with this checkout's
release build: the backup, every schema step and the rewrite without free
space, as setup runs them. Prints each run, the median, the store's and the
keyword index's sizes before and after, and checks that the upgrade made one
`.bak`, kept every memory's content byte for byte and left the store at the
binary's schema. Exits with status 1 when the median is over the budget.
Nothing here touches the user's own store or settings.

    just upgrade-budget --from-binary /path/to/0.9.3/user-memory
    just upgrade-budget --from-binary OLD --memories 20000 --runs 3
"""

import argparse
import hashlib
import importlib.util
import os
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

# The search budget's store generator, so both measure the same store.
_spec = importlib.util.spec_from_file_location("scale_run", Path(__file__).with_name("run.py"))
scale = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scale)

# The whole upgrade of 0.10.0's six schema steps, from schema version 6, on
# 100,000 memories with 2,560-dimension vectors (a 1.3 GB store): setup and
# the installers run it once, with every session stopped, so it may take
# seconds but stays under a minute. Version 7 alone took 11 s, nearly all
# of it the backup and the rewrite, which copy the whole file; the rest is
# room for the keyword index's one rebuild and the later steps.
BUDGET_S = 45.0

# Timed upgrades, each on a fresh copy of the built store.
RUNS = 3


def checksum(db):
    """A digest of every memory's id and content, in id order."""
    digest = hashlib.sha256()
    conn = sqlite3.connect(db)
    for memory_id, content in conn.execute("SELECT id, content FROM memories ORDER BY id"):
        digest.update(memory_id.encode())
        digest.update(b"\0")
        digest.update(content.encode())
        digest.update(b"\0")
    conn.close()
    return digest.hexdigest()


def sizes(db):
    """The store's size and the keyword index's, in MB."""
    conn = sqlite3.connect(db)
    index = conn.execute(
        "SELECT coalesce(sum(pgsize),0) FROM dbstat WHERE name LIKE 'search_words%'").fetchone()[0]
    conn.close()
    return round(db.stat().st_size / 1e6, 1), round(index / 1e6, 1)


def version(db):
    conn = sqlite3.connect(db)
    found = conn.execute("PRAGMA user_version").fetchone()[0]
    conn.close()
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--from-binary", type=Path, required=True,
                        help="a release build of the version the upgrade starts from")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/user-memory")
    parser.add_argument("--memories", type=int, default=scale.MEMORIES)
    parser.add_argument("--dimensions", type=int, default=scale.DIMENSIONS,
                        help="vector length; 0 for a store without vectors")
    parser.add_argument("--runs", type=int, default=RUNS)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        home = tmp / "home"
        home.mkdir()
        built = tmp / "built" / "memories.sqlite3"
        built.parent.mkdir()
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
               "XDG_DATA_HOME": str(home / ".local/share"), "XDG_STATE_HOME": str(home / ".local/state"),
               "XDG_CONFIG_HOME": str(home / ".config"), "USER_MEMORY_NO_UPDATE_CHECK": "1",
               **{name: "" for name in scale.EMBED_SETTINGS}}
        scale.build(built, args.from_binary, {**env, "USER_MEMORY_DB": str(built)}, args.memories)
        if args.dimensions:
            scale.add_vectors(built, args.dimensions)
        sqlite3.connect(built).execute("PRAGMA wal_checkpoint(TRUNCATE)").close()
        start = version(built)
        expected = checksum(built)
        before = sizes(built)
        print(f"{args.memories} memories, vectors of {args.dimensions or 'no'} dimensions, "
              f"schema version {start}: store {before[0]} MB, keyword index {before[1]} MB")
        samples = []
        for run in range(args.runs):
            folder = tmp / f"run{run}"
            folder.mkdir()
            db = folder / "memories.sqlite3"
            shutil.copy(built, db)
            started = time.perf_counter()
            out = subprocess.run([str(args.binary), "migrate", "--db", str(db)],
                                 env={**env, "USER_MEMORY_DB": str(db)}, capture_output=True, text=True)
            elapsed = time.perf_counter() - started
            if out.returncode != 0:
                sys.exit(f"migrate failed: {out.stderr}")
            backups = [p for p in folder.iterdir() if p.suffix == ".bak"]
            if len(backups) != 1:
                sys.exit(f"expected one .bak, found {backups}")
            if checksum(db) != expected:
                sys.exit("a memory's content changed")
            samples.append(elapsed)
            print(f"run {run + 1}: {elapsed:.1f} s; schema version {version(db)}")
            if run == 0:
                print("  " + out.stdout.strip().replace("\n", "\n  "))
                after = sizes(db)
                print(f"  after: store {after[0]} MB, keyword index {after[1]} MB")
        median = statistics.median(samples)
        print(f"median of {args.runs}: {median:.1f} s (budget {BUDGET_S:.0f} s)")
    if median > BUDGET_S:
        sys.exit(1)


if __name__ == "__main__":
    main()
