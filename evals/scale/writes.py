#!/usr/bin/env python3
"""The time record, update and forget take on a large store.

Builds a throwaway store of synthetic memories (100,000 unless told
otherwise) as `just search-budget` builds its store, then times each write
as a whole command-line process, the median of many runs, with this
checkout's release build and, on a copy, without the history's triggers,
so the triggers' cost shows on its own. Given `--baseline-binary`, an
earlier release's build (once per release), it times that release on a
store it built, for the numbers a change is compared with. Prints a table in
milliseconds, with each store's size and its keyword index's. Nothing here
touches the user's own store or settings.

    just write-budget
    just write-budget --baseline-binary /path/to/0.9.1/user-memory --baseline-binary /path/to/0.9.3/user-memory
"""

import argparse
import importlib.util
import os
import random
import shutil
import sqlite3
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

_spec = importlib.util.spec_from_file_location("scale_run", Path(__file__).with_name("run.py"))
scale = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scale)

# The upgrade budget's size measure, so both report sizes alike.
_spec = importlib.util.spec_from_file_location("scale_upgrade", Path(__file__).with_name("upgrade.py"))
upgrade = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(upgrade)

# Timed writes of each kind; the first is a warm-up, not counted. As many
# as the measurements of #77 and #87 took, so medians compare.
RUNS = 15

# A heavy year of history: corrections and confirmations a day, each kind,
# far more than any store seen so far, for the size the history reaches
# before its oldest events are pruned.
CHANGES_PER_DAY = 50
DAYS_KEPT = 365


def ids(db, count, seed):
    """`count` memory ids, the same ones for every store built alike."""
    conn = sqlite3.connect(db)
    found = [row[0] for row in conn.execute("SELECT id FROM memories ORDER BY rowid")]
    conn.close()
    return random.Random(seed).sample(found, count)


def timed(binary, env, args):
    started = time.perf_counter()
    out = subprocess.run([str(binary), *args], env=env, capture_output=True, text=True)
    elapsed = time.perf_counter() - started
    if out.returncode != 0 or '"stored":false' in out.stdout or '"updated":false' in out.stdout:
        raise RuntimeError(f"{args[0]} failed: {out.stdout} {out.stderr}")
    return elapsed


def writes(binary, env, db, runs):
    """The median record, update and forget, in ms."""
    targets = ids(db, 2 * runs, scale.SEED)
    record, update, forget = [], [], []
    for n in range(runs):
        # Words no other memory holds, so nothing similar holds it back.
        words = " ".join(f"zq{n}x{i}" for i in range(6))
        record.append(timed(binary, env, [
            "record", "--category", "general", "--scope", "global", "--title", words.capitalize(),
            "--content", words]))
        update.append(timed(binary, env, ["update", targets[n], "--content", f"Changed text {n}"]))
        forget.append(timed(binary, env, ["forget", targets[runs + n]]))
    return [round(statistics.median(samples[1:]) * 1000) for samples in (record, update, forget)]


def store(tmp, name, binary, env, memories):
    db = tmp / name / "memories.sqlite3"
    db.parent.mkdir()
    scale.build(db, binary, {**env, "USER_MEMORY_DB": str(db)}, memories)
    return db


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/user-memory")
    parser.add_argument("--baseline-binary", type=Path, action="append", default=[],
                        help="an earlier release's build, timed on a store it built; repeatable")
    parser.add_argument("--memories", type=int, default=scale.MEMORIES)
    parser.add_argument("--runs", type=int, default=RUNS)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        home = tmp / "home"
        home.mkdir()
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
               "XDG_DATA_HOME": str(home / ".local/share"), "XDG_STATE_HOME": str(home / ".local/state"),
               "XDG_CONFIG_HOME": str(home / ".config"), "USER_MEMORY_NO_UPDATE_CHECK": "1",
               **{name: "" for name in scale.EMBED_SETTINGS}}
        rows = []
        for n, baseline in enumerate(args.baseline_binary):
            name = subprocess.run([str(baseline), "--version"], capture_output=True,
                                  text=True).stdout.strip() or f"baseline {n + 1}"
            db = store(tmp, f"baseline{n}", baseline, env, args.memories)
            sized = upgrade.sizes(db)
            rows.append((name, writes(baseline, {**env, "USER_MEMORY_DB": str(db)}, db, args.runs), sized))
        db = store(tmp, "current", args.binary, env, args.memories)
        bare = tmp / "bare" / "memories.sqlite3"
        bare.parent.mkdir()
        shutil.copy(db, bare)
        conn = sqlite3.connect(bare)
        triggers = [row[0] for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'memory_event_%'")]
        for trigger in triggers:
            conn.execute(f"DROP TRIGGER {trigger}")
        conn.commit()
        conn.close()
        sized = upgrade.sizes(db)
        rows.append(("this build", writes(args.binary, {**env, "USER_MEMORY_DB": str(db)}, db, args.runs),
                     sized))
        if triggers:
            rows.append(("without history triggers",
                         writes(args.binary, {**env, "USER_MEMORY_DB": str(bare)}, bare, args.runs), sized))
        print(f"{args.memories} memories, median of {args.runs - 1} runs after a warm-up (ms); "
              "sizes of the store as built, in MB")
        print(f"{'':<26} {'record':>7} {'update':>7} {'forget':>7} {'store':>7} {'index':>7}")
        for name, (record, update, forget), (size, index) in rows:
            print(f"{name:<26} {record:>7} {update:>7} {forget:>7} {size:>7} {index:>7}")
        conn = sqlite3.connect(db)
        events = conn.execute("SELECT count(*), coalesce(sum(length(before)),0) FROM memory_events").fetchone() \
            if triggers else None
        conn.close()
        if events:
            print(f"history after the timed writes: {events[0]} events, {events[1]} bytes of replaced values")
            print(year_of_history(db))


def year_of_history(db):
    """The history's size after a year of synthetic changes: each day,
    [`CHANGES_PER_DAY`] corrections of a memory's content and as many
    confirmations, through the store's own triggers."""
    conn = sqlite3.connect(db)
    rng = random.Random(scale.SEED)
    found = [row[0] for row in conn.execute("SELECT id FROM memories")]
    for day in range(DAYS_KEPT):
        for change in rng.sample(found, CHANGES_PER_DAY):
            conn.execute("UPDATE memories SET content=content||' edited' WHERE id=?", (change,))
        for confirmed in rng.sample(found, CHANGES_PER_DAY):
            conn.execute("UPDATE memories SET last_confirmed_at=datetime('now',?) WHERE id=?",
                         (f"+{day} days", confirmed))
        conn.commit()
    conn.execute("VACUUM")
    count, size = conn.execute(
        "SELECT (SELECT count(*) FROM memory_events),"
        " (SELECT sum(pgsize) FROM dbstat WHERE name LIKE 'memory_events%')").fetchone()
    store = db.stat().st_size
    conn.close()
    return (f"a year of {CHANGES_PER_DAY} corrections and {CHANGES_PER_DAY} confirmations a day: "
            f"{count} events, {size / 1e6:.1f} MB with their index, of a {store / 1e6:.1f} MB store")


if __name__ == "__main__":
    main()
