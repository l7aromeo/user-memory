#!/usr/bin/env python3
"""Search speed on a large store, against the budgets of issue #11.

Builds a throwaway store of synthetic memories (100,000 unless told
otherwise), builds its keyword index with `user-memory reindex`, gives every
memory a current vector, then times `user-memory search` as a whole
command-line process and as one call to a long-running `user-memory serve`,
keyword-only and fused with meaning, for topics whose words few, some and
many memories hold. Query embeddings come from a stand-in endpoint in this
script that answers at once, so the times are user-memory's own. Prints each
median and exits with status 1 if one is over its budget. Nothing here
touches the user's own store or settings.

    just search-budget
    just search-budget --memories 20000
    just search-budget --dimensions 1024
    just search-budget --keep DIR      # keep the built store in DIR
    just search-budget --store FILE    # time a kept store again, unbuilt
"""

import argparse
import hashlib
import http.server
import json
import os
import random
import shutil
import sqlite3
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

# Budgets from issue #11, measured warm. An agent searches several times per
# task, inside model turns that take seconds; 100 ms keeps memory under a
# few percent of a turn, whatever the store's size.
SERVER_BUDGET_MS = 100
# A command-line search also starts a process (about 10 ms) and reads the
# store cold. A fused one reads every vector, which no budget bounds: it is
# reported, not checked.
CLI_KEYWORD_BUDGET_MS = 150

# Vector length: qwen3-embedding's default, the longest a default install
# stores.
DIMENSIONS = 2560
# Distinct vectors the memories share, so writing them is quick; every
# memory still gets its own row and is compared on its own.
VECTOR_POOL = 1_000
# The model name the vectors and the stand-in endpoint carry.
MODEL = "search-budget"

# The store size the budgets hold at: far beyond any store seen so far, so
# a budget kept here is kept everywhere.
MEMORIES = 100_000
# Timed searches per topic and mode; the first of each is a warm-up, not
# counted. Enough for a stable median.
RUNS = 9

# The synthetic store: a vocabulary drawn by Zipf's law as natural text is,
# a tenth of the memories in three projects, and a dozen categories.
VOCABULARY = 20_000
ZIPF_EXPONENT = 1.1
PROJECT_SHARE = 0.1
PROJECTS = ["github.com/example/ledger", "github.com/example/website", "github.com/example/api"]
CATEGORIES = ["programming", "tooling", "workflow", "communication", "testing", "security",
              "release", "documentation", "design", "operations", "data", "general"]
TYPES = ["preference", "fact", "decision", "constraint", "goal", "pattern"]
# Words per title and per content, as recorded memories run.
TITLE_WORDS = (4, 12)
CONTENT_WORDS = (15, 60)
# The title limit the store enforces, with room for the cut word.
TITLE_CHARS = 110
# Rows per insert transaction while the store is built.
BATCH = 5_000
# Fixed, so every run times the same store.
SEED = 11

# Sandbox: the binary sees only these settings.
EMBED_SETTINGS = (
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
)


def vocabulary(rng):
    """Pronounceable made-up words, so no stem joins two of them by chance."""
    syllables = [c + v for c in "bdfgklmnprstvz" for v in "aeiou"]
    words = set()
    while len(words) < VOCABULARY:
        words.add("".join(rng.choice(syllables) for _ in range(rng.randint(2, 4))))
    return sorted(words)


def zipf_weights():
    return [1 / (rank ** ZIPF_EXPONENT) for rank in range(1, VOCABULARY + 1)]


def build(db, binary, env, count):
    rng = random.Random(SEED)
    words = vocabulary(rng)
    weights = zipf_weights()
    phrase = lambda bounds: " ".join(rng.choices(words, weights, k=rng.randint(*bounds)))
    subprocess.run([str(binary), "search"], env=env, check=True, capture_output=True)
    conn = sqlite3.connect(db)
    for start in range(0, count, BATCH):
        rows = []
        for _ in range(min(BATCH, count - start)):
            project = rng.choice(PROJECTS) if rng.random() < PROJECT_SHARE else ""
            title = phrase(TITLE_WORDS).capitalize()[:TITLE_CHARS].rstrip()
            rows.append((str(uuid.UUID(int=rng.getrandbits(128), version=4)), rng.choice(TYPES),
                         rng.choice(CATEGORIES), title, phrase(CONTENT_WORDS),
                         "project" if project else "global", project))
        conn.executemany(
            "INSERT INTO memories(id,memory_type,category,title,content,scope,project) VALUES (?,?,?,?,?,?,?)",
            rows)
        conn.commit()
    conn.close()
    started = time.perf_counter()
    subprocess.run([str(binary), "reindex"], env=env, check=True, capture_output=True)
    return words, time.perf_counter() - started


def unit_vector(seed, dimensions):
    rng = random.Random(seed)
    values = [rng.gauss(0, 1) for _ in range(dimensions)]
    norm = sum(v * v for v in values) ** 0.5
    return [v / norm for v in values]


def add_vectors(db, dimensions):
    """Gives every memory a current vector: `source` is the text user-memory
    embeds a memory as, which these memories, without context, reason or
    evidence, have as `category (type): content`."""
    pool = [struct.pack(f"<{dimensions}f", *unit_vector(i, dimensions)) for i in range(VECTOR_POOL)]
    conn = sqlite3.connect(db)
    conn.execute("DELETE FROM memory_vectors")
    rows = conn.execute("SELECT rowid,id,category,memory_type,content FROM memories").fetchall()
    for start in range(0, len(rows), BATCH):
        conn.executemany(
            "INSERT INTO memory_vectors(id,model,dimensions,source,vector) VALUES (?,?,?,?,?)",
            [(id, MODEL, dimensions, f"{category} ({kind}): {content}", pool[rowid % VECTOR_POOL])
             for rowid, id, category, kind, content in rows[start:start + BATCH]])
        conn.commit()
    conn.close()


class Embedder(http.server.BaseHTTPRequestHandler):
    """An embeddings endpoint answering at once, with a vector picked by
    each input's hash from a pool made beforehand."""

    pool = []

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        pick = lambda text: int.from_bytes(hashlib.sha256(text.encode()).digest()[:8], "little")
        data = [{"index": i, "embedding": self.pool[pick(text) % len(self.pool)]}
                for i, text in enumerate(request["input"])]
        body = json.dumps({"data": data, "model": request["model"]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def topics(db, words):
    """Topics of words held by few, some and many memories, with how many
    memories hold each word."""
    conn = sqlite3.connect(db)
    held = lambda word: conn.execute(
        "SELECT COUNT(*) FROM memories WHERE ' '||lower(title)||' '||content||' ' LIKE ?",
        (f"% {word} %",)).fetchone()[0]
    chosen = {"rare": [words[2000], words[9000]], "some": [words[60], words[400]],
              "common": [words[0], words[3]]}
    found = {name: (" ".join(pair), [held(w) for w in pair]) for name, pair in chosen.items()}
    conn.close()
    return found


def median_ms(samples):
    return round(statistics.median(samples[1:]) * 1000, 1)


def time_cli(binary, env, topic):
    samples = []
    for _ in range(RUNS + 1):
        started = time.perf_counter()
        subprocess.run([str(binary), "search", topic, "--project", PROJECTS[0]], env=env,
                       check=True, capture_output=True)
        samples.append(time.perf_counter() - started)
    return median_ms(samples)


class Server:
    """One `user-memory serve` answering search calls over stdio."""

    def __init__(self, binary, env):
        self.process = subprocess.Popen([str(binary), "serve"], env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.next = 0
        self.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "search-budget", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, method, params):
        self.next += 1
        self.send({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})
        while True:
            reply = json.loads(self.process.stdout.readline())
            if reply.get("id") == self.next:
                return reply

    def search(self, topic):
        samples = []
        for _ in range(RUNS + 1):
            started = time.perf_counter()
            reply = self.call("tools/call", {"name": "search", "arguments": {
                "topic": topic, "projects": [PROJECTS[0]]}})
            samples.append(time.perf_counter() - started)
            if reply["result"].get("isError"):
                raise RuntimeError(reply["result"]["content"][0]["text"])
        return median_ms(samples)

    def rss_mb(self):
        """The server's resident memory, in MB."""
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.process.pid)],
                             capture_output=True, text=True).stdout.strip()
        return round(int(out) / 1024) if out else None

    def close(self):
        self.process.terminate()
        self.process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/user-memory")
    parser.add_argument("--memories", type=int, default=MEMORIES)
    parser.add_argument("--dimensions", type=int, default=DIMENSIONS)
    parser.add_argument("--keep", type=Path, help="keep the built store in this folder")
    parser.add_argument("--store", type=Path, help="time this store, kept before, instead of building one")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        home = tmp / "home"
        home.mkdir()
        db = tmp / "memories.sqlite3"
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
               "XDG_DATA_HOME": str(home / ".local/share"), "XDG_STATE_HOME": str(home / ".local/state"),
               "XDG_CONFIG_HOME": str(home / ".config"), "USER_MEMORY_DB": str(db),
               "USER_MEMORY_NO_UPDATE_CHECK": "1", **{name: "" for name in EMBED_SETTINGS}}
        if args.store:
            shutil.copy(args.store, db)
            for suffix in ("-wal", "-shm"):
                if Path(str(args.store) + suffix).exists():
                    shutil.copy(str(args.store) + suffix, str(db) + suffix)
            words = vocabulary(random.Random(SEED))
            subprocess.run([str(args.binary), "reindex"], env=env, check=True, capture_output=True)
            print(f"{args.store}: {sqlite3.connect(db).execute('SELECT COUNT(*) FROM memories').fetchone()[0]} memories")
        else:
            words, reindex_s = build(db, args.binary, env, args.memories)
            print(f"{args.memories} memories; reindex built the keyword index in {reindex_s:.1f} s")
            if args.keep:
                args.keep.mkdir(parents=True, exist_ok=True)
                sqlite3.connect(db).execute("VACUUM INTO ?", (str(args.keep / "memories.sqlite3"),))
        add_vectors(db, args.dimensions)
        Embedder.pool = [unit_vector(f"query {i}", args.dimensions) for i in range(16)]
        endpoint = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Embedder)
        threading.Thread(target=endpoint.serve_forever, daemon=True).start()
        fused_env = {**env, "USER_MEMORY_EMBED_URL": f"http://127.0.0.1:{endpoint.server_port}/v1/embeddings",
                     "USER_MEMORY_EMBED_MODEL": MODEL, "USER_MEMORY_EMBED_DIMENSIONS": str(args.dimensions),
                     "USER_MEMORY_EMBED_ALLOW_HTTP": "1"}
        over = []
        chosen = topics(db, words)
        times = {}
        for mode, mode_env in (("keyword", env), ("fused", fused_env)):
            # The command line first, with no server holding the store open,
            # as a search from a shell runs.
            for name, (topic, _) in chosen.items():
                times[mode, "cli", name] = time_cli(args.binary, mode_env, topic)
            server = Server(args.binary, mode_env)
            try:
                started = time.perf_counter()
                server.call("tools/call", {"name": "search", "arguments": {"topic": "warm up"}})
                times[mode, "first", None] = round((time.perf_counter() - started) * 1000)
                for name, (topic, _) in chosen.items():
                    times[mode, "server", name] = server.search(topic)
                times[mode, "rss", None] = server.rss_mb()
            finally:
                server.close()
        endpoint.shutdown()
        print(f"vectors: {args.dimensions} dimensions; the fused server's first search, which reads "
              f"them, took {times['fused', 'first', None]} ms; server memory {times['fused', 'rss', None]} MB "
              f"fused, {times['keyword', 'rss', None]} MB keyword-only")
        print(f"{'topic':<8} {'memories holding its words':<28} {'keyword CLI':>12} {'server':>8}"
              f" {'fused CLI':>10} {'server':>8}   (ms)")
        for name, (_, held) in chosen.items():
            row = [times[mode, where, name] for mode in ("keyword", "fused") for where in ("cli", "server")]
            print(f"{name:<8} {', '.join(map(str, held)):<28} {row[0]:>12} {row[1]:>8} {row[2]:>10} {row[3]:>8}")
            if row[0] > CLI_KEYWORD_BUDGET_MS:
                over.append(f"keyword {name}: CLI {row[0]} ms > {CLI_KEYWORD_BUDGET_MS} ms")
            for mode, served in (("keyword", row[1]), ("fused", row[3])):
                if served > SERVER_BUDGET_MS:
                    over.append(f"{mode} {name}: server {served} ms > {SERVER_BUDGET_MS} ms")
    if over:
        print("Over budget:\n  " + "\n  ".join(over))
        sys.exit(1)
    print(f"Within budget: CLI keyword under {CLI_KEYWORD_BUDGET_MS} ms, server keyword and fused "
          f"under {SERVER_BUDGET_MS} ms")


if __name__ == "__main__":
    main()
