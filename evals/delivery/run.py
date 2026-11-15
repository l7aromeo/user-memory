#!/usr/bin/env python3
"""How well delivery matching (src/memory/delivery.rs) finds what whole
tasks need, on synthetic stores of 50, 1,000 and 10,000 memories (issue
#125). Nothing here touches the user's own store or settings: each store
is built in a throwaway home by the real binary.

For each size and seed it draws a world (evals/delivery/world.py), writes
its memories into a new store and builds the keyword index with
`user-memory reindex`, builds the delivery index with `user-memory-bench
index`, runs every task's actions through `user-memory-bench match`, and
reports precision and recall of what the bars deliver
(evals/delivery/metrics.py).

    just delivery-bench                       # the report, on the report seeds
    just delivery-bench --sizes 50            # one size (CI)
    just delivery-bench --sweep               # bars and window on the tuning seeds
    just delivery-bench --cost                # one lookup's time, 10,000 memories
    just delivery-bench --credit 0.25         # other passages' credit
"""

import argparse
import json
import os
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
import metrics  # noqa: E402
import presence  # noqa: E402
import world  # noqa: E402

SIZES = [50, 1_000, 10_000]
# Seeds the constants were set on, and seeds the report is given on; the
# two never overlap, so the report isn't graded on what tuned it.
TUNING_SEEDS = [1, 2, 3]
REPORT_SEEDS = [11, 12, 13]
# The bars a sweep tries.
PUSH_BARS = [1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 5.0]
FAILURE_BARS = [0.5, 0.75, 1.0, 1.5, 2.0]
HOLD_BARS = [1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 5.0]
# The window settings a sweep tries.
WINDOWS = [10, 30]
HALF_LIVES = [2.0, 5.0, 10.0]
# Whether a push needs an anchor of the action (#125's named change).
ANCHOR_RULES = [False, True]
# Budgets of #125.
PUSH_PRECISION = 0.8
PROMPT_PRECISION = 0.9
LOOKUP_LIMIT_MS = 100
# Lookups timed per action for the cost, after one warm-up.
LOOKUPS = 21


def sandbox(home):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
           "XDG_DATA_HOME": str(home / ".local/share"), "XDG_STATE_HOME": str(home / ".local/state"),
           "XDG_CONFIG_HOME": str(home / ".config"), "USER_MEMORY_NO_UPDATE_CHECK": "1",
           "USER_MEMORY_DB": str(home / "memories.sqlite3")}
    env.update({name: "" for name in world.scale.EMBED_SETTINGS})
    for name in ("SYSTEMROOT", "TEMP", "TMP", "USERPROFILE", "APPDATA", "LOCALAPPDATA"):
        if name in os.environ:
            env[name] = os.environ[name]
    return env


def build(folder, binary, drawn):
    """A store of `drawn`'s memories in `folder`, indexed by the binary.
    Returns its path and the seconds building took."""
    home = folder / "home"
    home.mkdir(parents=True)
    env = sandbox(home)
    db = Path(env["USER_MEMORY_DB"])
    started = time.perf_counter()
    subprocess.run([str(binary), "search"], env=env, check=True, capture_output=True)
    conn = sqlite3.connect(db)
    conn.executemany(
        "INSERT INTO memories(id,memory_type,category,title,content,context,scope,project) "
        "VALUES (:id,:type,:category,:title,:content,:context,:scope,:project)",
        drawn["memories"])
    conn.commit()
    conn.close()
    subprocess.run([str(binary), "reindex"], env=env, check=True, capture_output=True)
    return db, time.perf_counter() - started


def bench(bench_binary, *args):
    out = subprocess.run([str(bench_binary), *map(str, args)], check=True, capture_output=True, text=True)
    return out.stdout


def match(bench_binary, index, drawn, folder, tuning):
    tasks = folder / "tasks.json"
    tasks.write_text(json.dumps([{"id": t["id"], "actions": t["actions"]} for t in drawn["tasks"]]))
    args = ["match", "--index", index, "--tasks", tasks]
    for flag, value in tuning.items():
        if value is not None:
            args += [f"--{flag}", value]
    matched = {}
    for line in bench(bench_binary, *args).splitlines():
        row = json.loads(line)
        matched.setdefault(row["task"], []).append(row["matched"])
    return matched


def run_world(binaries, seed, size, tuning, keep=None, then=None):
    """One world's tasks, matched: the drawn world, what each action matched,
    the store's size and the seconds building it took. `then(env, folder,
    drawn, matched)` runs while the store still exists; its result comes
    last."""
    drawn = world.generate(seed, size)
    with tempfile.TemporaryDirectory() as tmp:
        folder = Path(keep or tmp) / f"seed{seed}-size{size}"
        folder.mkdir(parents=True, exist_ok=True)
        db, seconds = build(folder, binaries["user-memory"], drawn)
        index = folder / "delivery.index"
        bench(binaries["bench"], "index", "--db", db, "--project", world.PROJECT, "--out", index)
        matched = match(binaries["bench"], index, drawn, folder, tuning)
        extra = then(sandbox(folder / "home"), folder, drawn, matched) if then else None
    return drawn, matched, seconds, extra


def counts(drawn, matched, bars):
    memories = {m["id"]: m for m in drawn["memories"]}
    total = Counter()
    by_kind = {}
    for task in drawn["tasks"]:
        if task.get("probe"):
            # A split-words probe: found when its long memory ranks first.
            found = matched[task["id"]][0]
            wanted = [c for c in found if c["id"] == task["needs"][0]["memory"]]
            total["split probes"] += 1
            total["split found"] += bool(wanted) and all(
                c["score"] < wanted[0]["score"] for c in found if c is not wanted[0])
            continue
        c = metrics.evaluate(task, matched[task["id"]], memories, bars)
        total.update(c)
        by_kind.setdefault(task["kind"], Counter()).update(c)
    return total, by_kind


def check_constants(drawn, matched, bars):
    """The script's decisions at the matcher's own bars equal the matcher's."""
    kinds = {t["id"]: [a["kind"] for a in t["actions"]] for t in drawn["tasks"]}
    for task, actions in matched.items():
        for i, candidates in enumerate(actions):
            for c in candidates:
                mine = metrics.decisions(c, bars, kinds[task][i])
                theirs = {way for way in ("push", "failure", "hold") if c[way]}
                if mine != theirs:
                    raise SystemExit(f"bars disagree on {task} action {i}: {c} ({mine} vs {theirs})")


def fmt(value):
    return "  -  " if value is None else f"{value:5.2f}"


def report(title, total, by_kind):
    r = metrics.ratio
    print(f"\n{title}")
    print(f"  pushes: precision {fmt(r(total['right|all'], total['delivered|all']))} of "
          f"{total['delivered|all']}, at the prompt {fmt(r(total['right|signal:prompt'], total['delivered|signal:prompt']))} "
          f"of {total['delivered|signal:prompt']}; recall {fmt(r(total['found|all'], total['needed|all']))} "
          f"of {total['needed|all']} needs")
    print("  by way      " + "  ".join(
        f"{w} {fmt(r(total[f'right|way:{w}'], total[f'delivered|way:{w}']))}/{total[f'delivered|way:{w}']}"
        for w in ("push", "failure", "hold")))
    print("  by signal   " + "  ".join(
        f"{s} {fmt(r(total[f'right|signal:{s}'], total[f'delivered|signal:{s}']))}/{total[f'delivered|signal:{s}']}"
        for s in ("prompt", "command", "file", "message", "search", "plan", "task")
        if total[f"delivered|signal:{s}"]))
    print("  shape       precision      recall")
    for shape in ("note", "lesson", "decision", "rule", "merged", "look-alike", "filler"):
        p = r(total[f"right|shape:{shape}"], total[f"delivered|shape:{shape}"])
        n = total[f"needed|shape:{shape}"]
        if total[f"delivered|shape:{shape}"] or n:
            print(f"    {shape:<10} {fmt(p)} /{total[f'delivered|shape:{shape}']:<5} "
                  f"{fmt(r(total[f'found|shape:{shape}'], n))} /{n}")
    print("  subject     recall   " + "  ".join(metrics.__dict__[w].ljust(7) for w in ("EARLY", "ON_TIME", "LATE", "NEVER")))
    for subject in ("command", "failure", "file", "component", "rule"):
        n = total[f"needed|subject:{subject}"]
        if n:
            timing = "  ".join(f"{total[f'timing {w}|subject:{subject}']:<7}"
                               for w in (metrics.EARLY, metrics.ON_TIME, metrics.LATE, metrics.NEVER))
            print(f"    {subject:<10}{fmt(r(total[f'found|subject:{subject}'], n))} /{n:<4} {timing}")
    print("  wrong deliveries: about this action but not needed in the task / about something else")
    for group in ["all"] + [f"shape:{s}" for s in ("note", "lesson", "decision", "rule", "merged",
                                                    "look-alike", "filler")]:
        wrong = total[f"delivered|{group}"] - total[f"right|{group}"]
        if wrong:
            print(f"    {group.removeprefix('shape:'):<10} {total[f'wrong about|{group}']:>4} / "
                  f"{total[f'wrong elsewhere|{group}']:<4}")
    print(f"  precision both ways: needed in the task {fmt(r(total['right|all'], total['delivered|all']))}, "
          f"about this action {fmt(r(total['right|all'] + total['wrong about|all'], total['delivered|all']))}")
    moments = (metrics.BEFORE_FAILURE, metrics.AT_FAILURE, metrics.AFTER_FAILURE,
               metrics.BEFORE_SUCCESS, metrics.AFTER_SUCCESS, metrics.NOT_RUN)
    if any(total[f"lesson {m}|{v}"] for m in moments for v in ("right", "wrong")):
        print("  lessons shown, right / wrong:")
        for m in moments:
            if total[f"lesson {m}|right"] or total[f"lesson {m}|wrong"]:
                print(f"    {m:<40} {total[f'lesson {m}|right']:>4} / {total[f'lesson {m}|wrong']}")
    print(f"  prompts naming a literal {total['prompts literal']} of {total['prompts']}; prompt "
          f"deliveries right: with a literal {fmt(r(total['right|prompt literal'], total['delivered|prompt literal']))}"
          f"/{total['delivered|prompt literal']}, without {fmt(r(total['right|prompt plain'], total['delivered|prompt plain']))}"
          f"/{total['delivered|prompt plain']}")
    reasons = (metrics.NO_ANCHOR, metrics.NOT_MATCHED, metrics.SPELLED, metrics.BELOW)
    print("  misses by reason: " + " / ".join(reasons))
    for subject in ("command", "failure", "file", "component", "rule", "start rule"):
        n = total[f"needed|subject:{subject}"]
        if n:
            print(f"    {subject:<10} " + " / ".join(
                f"{total[f'missed {why}|subject:{subject}']}" for why in reasons)
                  + f"   of {n - total[f'found|subject:{subject}']} missed")
    print(f"  rules that name no action (outside the budget): pushed {total['delivered|shape:start rule']}, "
          f"right {total['right|shape:start rule']}; needed {total['needed|subject:start rule']}, "
          f"found {total['found|subject:start rule']}")
    print(f"  failures: found at the failed action {total['failure found at its action']} of "
          f"{total['failure needs']}; failure-only pushes right "
          f"{total['right|way:failure']} of {total['delivered|way:failure']}")
    print(f"  holds: needed command passages held before their command {total['held before its action']} "
          f"of {total['command needs']}; commands no memory names held {total['unnamed held']} of "
          f"{total['unnamed commands']}")
    if total["split probes"]:
        print(f"  split words: the long memory ranks first for {total['split found']} of "
              f"{total['split probes']} searches naming a word of each half of its point")
    print(f"  long against short: a needed short memory outranked by a long memory's passage not "
          f"needed then {total['long over short']} of {total['short needed']}")
    if total["merged delivered"]:
        print(f"  long memories: delivered passages holding the needed point whole "
              f"{total['merged whole']} of {total['merged delivered']}")
    print("  by kind of work: precision / recall")
    for kind in world.KINDS:
        c = by_kind.get(kind, Counter())
        print(f"    {kind:<9} {fmt(r(c['right|all'], c['delivered|all']))} /{c['delivered|all']:<5}"
              f" {fmt(r(c['found|all'], c['needed|all']))} /{c['needed|all']}")


def binaries(release):
    profile = "release" if release else "debug"
    exe = ".exe" if os.name == "nt" else ""
    found = {"user-memory": ROOT / "target" / profile / f"user-memory{exe}",
             "bench": ROOT / "target" / profile / f"user-memory-bench{exe}"}
    for path in found.values():
        if not path.exists():
            raise SystemExit(f"{path} is missing; `just delivery-bench` builds it")
    return found


def bars_of_matcher():
    """The matcher's own bars, read from its source so the report states them."""
    source = (ROOT / "src/memory/delivery.rs").read_text()
    bars = {}
    for name, key in (("PUSH_BAR", "push"), ("FAILURE_BAR", "failure"), ("HOLD_BAR", "hold")):
        line = next(l for l in source.splitlines() if l.startswith(f"pub(crate) const {name}: f64 = "))
        bars[key] = float(line.split("=")[1].strip(" ;"))
    return bars


def sweep(found, sizes, seeds, tuning, keep):
    """Precision and recall over bars and window settings, on the tuning
    seeds: the rows the constants are set from."""
    print("\nsweep on seeds", seeds)
    print("size  window half-life anchor push  failure hold   precision prompt  recall  held  unnamed-held")
    for size in sizes:
        for window in WINDOWS:
            for half_life in HALF_LIVES:
                runs = [run_world(found, seed, size, {**tuning, "window": window, "half-life": half_life}, keep)
                        for seed in seeds]
                for anchor in ANCHOR_RULES:
                  for push in PUSH_BARS:
                    for failure in [b for b in FAILURE_BARS if b <= push]:
                        for hold in [b for b in HOLD_BARS if b >= push]:
                            bars = {"push": push, "failure": failure, "hold": hold, "anchor": anchor}
                            total = Counter()
                            for drawn, matched, _, _ in runs:
                                total.update(counts(drawn, matched, bars)[0])
                            r = metrics.ratio
                            print(f"{size:<6}{window:<7}{half_life:<10}{anchor!s:<7}{push:<6}{failure:<8}{hold:<7}"
                                  f"{fmt(r(total['right|all'], total['delivered|all']))}     "
                                  f"{fmt(r(total['right|signal:prompt'], total['delivered|signal:prompt']))}  "
                                  f"{fmt(r(total['found|all'], total['needed|all']))}   "
                                  f"{total['held before its action']:>4}  {total['unnamed held']:>4}")


def cost(found, seed, size, keep):
    """One lookup's time from a new process: start, load the delivery
    index, match one action; and with the passing passages' text cut from
    the store."""
    drawn = world.generate(seed, size)
    with tempfile.TemporaryDirectory() as tmp:
        folder = Path(keep or tmp) / f"cost-seed{seed}-size{size}"
        folder.mkdir(parents=True, exist_ok=True)
        db, seconds = build(folder, found["user-memory"], drawn)
        index = folder / "delivery.index"
        built = json.loads(bench(found["bench"], "index", "--db", db, "--project", world.PROJECT, "--out", index))
        actions = [a for t in drawn["tasks"] for a in t["actions"] if a["kind"] in ("command", "file")][:10]
        times = {"index only": [], "with text": []}
        for action in actions:
            for mode, extra in (("index only", []), ("with text", ["--db", db])):
                args = [found["bench"], "lookup", "--index", index, "--action", json.dumps(action), *extra]
                for run in range(LOOKUPS):
                    started = time.perf_counter()
                    subprocess.run(list(map(str, args)), check=True, capture_output=True)
                    if run:
                        times[mode].append((time.perf_counter() - started) * 1000)
        print(f"\nlookup cost: {size} memories (store built in {seconds:.1f} s; delivery index "
              f"{built['bytes'] / 1e6:.1f} MB), {len(actions)} actions x {LOOKUPS - 1} runs, {sys.platform}")
        worst = 0.0
        for mode, samples in times.items():
            samples.sort()
            median = statistics.median(samples)
            p95 = samples[int(len(samples) * 0.95) - 1]
            worst = max(worst, median)
            print(f"  {mode:<11} median {median:6.1f} ms, 95th percentile {p95:6.1f} ms, "
                  f"largest {samples[-1]:6.1f} ms")
        return worst


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sizes", default=",".join(map(str, SIZES)))
    parser.add_argument("--seeds", help="seeds to report on (default: the report seeds)")
    parser.add_argument("--sweep", action="store_true", help="sweep bars and window on the tuning seeds")
    parser.add_argument("--cost", action="store_true", help="time one lookup on the largest size")
    parser.add_argument("--check-cost", action="store_true", help="fail when the lookup's median is over its limit")
    parser.add_argument("--credit", type=float, help="credit for terms only other passages hold")
    parser.add_argument("--bars", action="append",
                        help="PUSH,FAILURE,HOLD[,anchor] to report at instead of the matcher's; repeatable")
    parser.add_argument("--presence", action="store_true",
                        help="how often session start lists what tasks need (evals/delivery/presence.py)")
    parser.add_argument("--prompt-curve", help="prompt bars, on stems alone, to report prompt pushes at")
    parser.add_argument("--debug", action="store_true", help="use the debug build")
    parser.add_argument("--keep", type=Path, help="keep the built stores in this folder")
    args = parser.parse_args()
    found = binaries(not args.debug)
    sizes = [int(s) for s in args.sizes.split(",")]
    tuning = {"credit": args.credit}
    if args.cost or args.check_cost:
        worst = cost(found, REPORT_SEEDS[0], max(sizes), args.keep)
        if args.check_cost and worst > LOOKUP_LIMIT_MS:
            print(f"over the {LOOKUP_LIMIT_MS} ms limit")
            return 1
        return 0
    if args.sweep:
        sweep(found, sizes, [int(s) for s in args.seeds.split(",")] if args.seeds else TUNING_SEEDS,
              tuning, args.keep)
        return 0
    seeds = [int(s) for s in args.seeds.split(",")] if args.seeds else REPORT_SEEDS
    variants = [bars_of_matcher()]
    if args.bars:
        variants = []
        for given in args.bars:
            parts = given.split(",")
            variants.append({"push": float(parts[0]), "failure": float(parts[1]),
                             "hold": float(parts[2]), "anchor": parts[3:] == ["anchor"]})
    print(f"delivery matching on seeds {seeds}" + (f"; credit {args.credit}" if args.credit else ""))
    for size in sizes:
        def present(env, folder, drawn, matched, seed):
            return presence.presence(found["user-memory"], env, folder, drawn, matched, variants[0], seed)

        runs = [run_world(found, seed, size, tuning, args.keep,
                          (lambda *a, seed=seed: present(*a, seed)) if args.presence else None)
                for seed in seeds]
        built = ", ".join(f"{len(drawn['memories'])} memories in {s:.1f} s" for drawn, _, s, _ in runs)
        for bars in variants:
            total, by_kind = Counter(), {}
            for drawn, matched, _, _ in runs:
                if args.credit is None and not args.bars:
                    check_constants(drawn, matched, bars)
                c, k = counts(drawn, matched, bars)
                total.update(c)
                for kind, value in k.items():
                    by_kind.setdefault(kind, Counter()).update(value)
            report(f"{size} memories ({built}); bars push {bars['push']}, failure {bars['failure']}, "
                   f"hold {bars['hold']}" + ("; a push needs an anchor" if bars.get("anchor") else ""),
                   total, by_kind)
        if args.presence:
            report_presence(size, runs)
        if args.prompt_curve:
            print(f"\n  prompt bar on stems alone, {size} memories, other bars as {variants[0]}:")
            print("    bar   precision  of     recall of the prompt's needs")
            for bar in map(float, args.prompt_curve.split(",")):
                bars = {**variants[0], "prompt": bar}
                total = Counter()
                for drawn, matched, _, _ in runs:
                    total.update(counts(drawn, matched, bars)[0])
                print(f"    {bar:<5} {fmt(metrics.ratio(total['right|signal:prompt'], total['delivered|signal:prompt']))}"
                      f"      {total['delivered|signal:prompt']:<6} "
                      f"{fmt(metrics.ratio(total['found|subject:component'], total['needed|subject:component']))}")
    return 0


def report_presence(size, runs):
    """Session start's share of each subject's needs, today's and the
    model's, summed over the seeds."""
    r = metrics.ratio
    first = Counter()
    rows = {}
    for *_, (today_counts, model) in runs:
        first.update(today_counts)
        for key, counts in model:
            rows.setdefault(key, Counter()).update(counts)
    subjects = ("component", "command", "file", "failure", "rule", "start rule", "all")
    print(f"\n  session start lists the needed memory, {size} memories: share of needs")
    print("    " + " " * 42 + "  ".join(f"{s:<9}" for s in subjects))
    print(f"    {"today, first sessions":<42}" + "  ".join(
        f"{fmt(r(first[f'listed first|{s}'], first[f'needed first|{s}'])):<9}" for s in subjects))
    for key, counts in rows.items():
        if key is None:
            label = "model, no use yet"
        else:
            reads, window, share = key
            label = f"{'delivered or read' if reads else 'delivered'}, window {window}, share {share}"
        print(f"    {label:<42}" + "  ".join(
            f"{fmt(r(counts[f'listed later|{s}'], counts[f'needed later|{s}'])):<9}" for s in subjects))


if __name__ == "__main__":
    sys.exit(main())
