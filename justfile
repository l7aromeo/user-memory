# Task runner for user-memory. `just` on its own lists every recipe.
#
# `just --list` shows only the comment line directly above a recipe, so each one
# carries a single-line summary; anything longer sits above a blank line.

# On Windows the recipes run in Windows PowerShell, which every Windows has.
# It has no `&&`, so a recipe runs its commands one per line: just stops at
# the first that fails.
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

default:
    @just --list

# ─── Quality ─────────────────────────────────────────────────────────────────

# What the project asks for before a commit: formatting, lints as errors, every test.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo nextest run --locked

# A release build compiles out the debug-only test seams, which can leave a
# variable, import or function unused there and nowhere else. CI runs this
# after `just check` on every platform it checks; `just check` leaves it out
# to stay fast, since it checks every crate again in the release profile.

# Lints as errors in the release profile, as users get the binary: CI runs it after `just check`.
check-release:
    cargo clippy --release --locked --all-targets -- -D warnings

# nextest runs each test in its own process and the test binaries side by
# side. It runs no doctests, and the crate has none to run: it is a binary
# without a library, and rustdoc tests only libraries.

# Every test through cargo-nextest; extra arguments pass through: `just test --test schema`.
test *args:
    cargo nextest run --locked {{ args }}

# CARGO_TARGET_DIR is unset: exported, it makes cargo-mutants build its
# mutated binaries into that folder, over the build the tests use. 3 jobs
# was the fastest here without false timeouts (docs/dev/development.md).

# Mutation testing of the Rust code, 3 mutants at a time; settings in .cargo/mutants.toml.
mutants *args:
    env -u CARGO_TARGET_DIR cargo mutants --jobs 3 {{ args }}

# Mutation testing of SQL string literals, which cargo-mutants never touches: `just sql-mutants --check`.
sql-mutants *args:
    scripts/sql-mutants {{ args }}

# The eval harnesses' own checks, which run no session: the agent eval's, and the retrieval benchmark's metrics, cases and private mode.
eval-tests:
    cargo build --locked
    python3 -I evals/test_run.py
    python3 -I evals/retrieval/test_run.py

# Search quality on synthetic memories, keyword-only unless given an endpoint: `just retrieval-bench --embed-url http://127.0.0.1:11434/v1/embeddings`.
retrieval-bench *args:
    cargo build --locked
    python3 -I evals/retrieval/run.py {{ args }}

# Delivery matching on synthetic whole tasks (issue #125): `just delivery-bench --sizes 50`, `--sweep`, `--cost`.
delivery-bench *args:
    cargo build --release --locked --bin user-memory --bin user-memory-bench
    python3 -I evals/delivery/run.py {{ args }}

# Search speed and the stored-vector count on 100,000 synthetic memories against their budgets; fails past one: `just search-budget --memories 20000`.
search-budget *args:
    cargo build --release --locked
    python3 -I evals/scale/run.py {{ args }}

# Review speed on 10,000 and 50,000 synthetic memories, keyword-only and with vectors, against its budgets; fails past one: `just review-budget --memories 20000 --dimensions 1024`.
review-budget *args:
    cargo build --release --locked
    python3 -I evals/scale/review.py {{ args }}

# The time a store upgrade takes on 100,000 synthetic memories, from a store an earlier release built: `just upgrade-budget --from-binary OLD`.
upgrade-budget *args:
    cargo build --release --locked
    python3 -I evals/scale/upgrade.py {{ args }}

# Record, update and forget times on 100,000 synthetic memories, with and without the history triggers: `just write-budget --baseline-binary OLD`.
write-budget *args:
    cargo build --release --locked
    python3 -I evals/scale/writes.py {{ args }}

# ─── This machine ────────────────────────────────────────────────────────────

# Builds from this checkout and connects the local clients: `just setup --url … --token-file …`.
setup *args:
    ./setup {{ args }}

# Checks the installed binary, its store or server, and every connected client.
doctor:
    user-memory doctor

# The Docker server on this machine (compose.yaml at the repository root), for trying things out.
local-up:
    docker compose up -d --build --wait

# Stops the local Docker server; its volume, and the memories in it, are kept.
local-down:
    docker compose down

# ─── Releases ────────────────────────────────────────────────────────────────

# Builds and checks a client release in a folder, publishing nothing (the release dry run): `just client-release --out DIR`.
client-release *args:
    scripts/client-release {{ args }}
