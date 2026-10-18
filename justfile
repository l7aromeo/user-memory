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
