# Development

Working on user-memory itself. Installing and running it is covered by
[install](../install.md) and [running the server](../server.md); what every
change needs is in [contributing](../../CONTRIBUTING.md), and how a release
is made in [releasing](releasing.md).

## Build and check

Requirements: Rust 1.99.1, the version the checks and the release builds use
(the Dockerfile's `RUST_IMAGE`), and a C build toolchain (SQLite is bundled).

```bash
cargo install cargo-nextest --locked   # once
just check
```

`just check` is what a commit must pass: `cargo fmt --check`, clippy with
warnings as errors, and every test through
[cargo-nextest](https://nexte.st/) (`cargo nextest run --locked`). nextest
runs each test in its own process and the test binaries side by side, which
took a quarter off `just check` here (74 s to 56 s, median of three). `just
test` runs the tests alone and passes arguments on, as `just test --test
schema`. `cargo test --locked` still runs the same tests. A local run stops
at the first failing test; the workflows set `NEXTEST_PROFILE=ci`, whose
profile in `.config/nextest.toml` runs every test, reports every failure, and
ends any test still running after 5 minutes as a failure. Every workflow job
also has a `timeout-minutes`, about twice its longest green run and at
least 5 minutes; the workflow files give the numbers. The crate has no
doctests: it is a binary without a library, and rustdoc tests only
libraries.

The tests drive the real binary with throwaway stores and fake clients; none
touch your real `~/.claude`, `~/.codex`, skills folders or memory store.

The programs user-memory and its scripts call (`claude`, `codex`, `docker`,
`brew`, `ollama`, `uname`, and for the release script `gh`, `cargo`,
`rustc` and `rustup`) are one fake: the `user-memory-fake` binary,
`tests/fake/cli.rs`. A test puts a copy on its PATH under the program's name,
such as `cli/codex` (`cli\codex.exe` on Windows), with `fake()` from
`tests/support/fake.rs`, and the copy's name says what it acts as. It keeps
its state in files under the test's `HOME` and logs its calls there, so a
test arranges a case by writing a file and checks one by reading it; the
doc comment on each program's function lists the files and switches it
reads; `claude --version` and `codex --version`, for example, print
`<name>.version` when a test writes one. A `<name>.exit` file beside a copy makes it fail at once with that
exit code. The fake is a binary rather than shell scripts so the same tests
can run on Windows. The release script and the Docker image build only
`--bin user-memory`, so it never ships; the image's build context leaves its
source out.

- `tests/support/releases.rs` serves this checkout's `install.sh` and this
  build as a release over loopback, for the installer tests.
- `tests/support/port.rs` gives each test a port no other test or process
  takes: below the system's ephemeral range (read from `sysctl`, `/proc` or
  `netsh`), reserved across processes with a lock file.

- `tests/acceptance.rs` — end-to-end user scenarios.
- `tests/fixtures/server/` — the `compose.yaml` and `Dockerfile` the shell
  installer wrote before `src/server.rs` took the work over; the module's
  unit tests require the same bytes, so an installed server never changes
  unnoticed.
- `tests/schema.rs` and `tests/fixtures/schema/` — upgrade guards: the current
  schema must match its committed snapshot, and a store built from every
  earlier snapshot must migrate to exactly the current schema while keeping
  all of its rows.
- `tests/skill.rs` — every tool, parameter and CLI flag the skill mentions
  exists in the binary.
- `tests/docker_context.rs` — every file the release binary embeds is inside
  the Docker build context.
- `tests/windows_install.rs` — `install.ps1` and `uninstall.ps1` under
  PowerShell 7 (`pwsh`): they parse, PSScriptAnalyzer's default rules find
  nothing (each suppression sits in the script with its reason), and help,
  refusals, the build choice, the checksum and the uninstaller's stand-in
  binary behave as on Windows. Without `pwsh` or the PSScriptAnalyzer
  module the tests skip with a note; in CI, or with `REQUIRE_PWSH=1`, they
  fail instead. The check workflow installs PSScriptAnalyzer at the version
  tests/ci.rs pins; locally, `Install-Module PSScriptAnalyzer -Scope
  CurrentUser` in `pwsh`. They run under the PowerShell that
  `USER_MEMORY_TEST_POWERSHELL` names instead, such as `powershell` for
  Windows PowerShell 5.1, which exists only on Windows.
- `tests/windows_setup.rs` — setup, doctor and uninstall on Windows only:
  a client found through PATHEXT as an npm `.cmd` shim, each client's hook
  form run as that client runs it, the user PATH entry, and an upgrade
  while `serve` runs. Each test edits a registry key of its own, named by
  `USER_MEMORY_ENVIRONMENT_KEY`, instead of your real PATH. That variable is
  for tests only: debug builds, which the tests run, read it, and a release
  build always edits `HKEY_CURRENT_USER\Environment`.
- Opt-in tests run only with their environment set: semantic calibration
  (`USER_MEMORY_EMBED_URL`), a real Codex (`USER_MEMORY_TEST_CODEX`), a
  Docker round trip (`USER_MEMORY_TEST_DOCKER=1`), and the installer's Docker
  server run for real, for a client install and for a server-only host
  (`USER_MEMORY_TEST_DOCKER_BINARY`, a Linux build for the Docker daemon's
  architecture). The `docker` workflow runs those on GitHub's Ubuntu runners;
  to run them here:

  ```bash
  zsh scripts/client-release --build-only --target aarch64-unknown-linux-gnu --out /tmp/user-memory-linux
  USER_MEMORY_TEST_DOCKER_BINARY=/tmp/user-memory-linux/user-memory-aarch64-unknown-linux-gnu cargo test --test install a_real_docker_server
  ```

### Windows

The `windows` workflow runs `just check` on Windows x64 (`windows-latest`) and
ARM64 (`windows-11-arm`), on every push to main and on pull requests that
change code, tests, the build, scripts, workflows or the PowerShell
installers; a docs-only change skips it. There the recipes run in Windows
PowerShell (the justfile's `windows-shell`), the install.ps1 tests are
required (`REQUIRE_PWSH`), and x64 uses aws-lc-sys's prebuilt NASM, which
the image lacks. After `just check`, the install.ps1 tests run again under
Windows PowerShell 5.1 (`USER_MEMORY_TEST_POWERSHELL=powershell`), which
keeps its own PSScriptAnalyzer, installed at the same pin. A `.gitattributes` keeps every checkout's line endings LF,
so the binary embeds the same skill files on every platform.

Tests that are about POSIX scripts or Unix files run only on Unix: the
installer, project, release and setup test files as a whole
(`#![cfg(unix)]`), and single tests whose `#[cfg(unix)]` says why. The fake
Docker's container shell needs a POSIX `sh`; on Windows it uses Git for
Windows' (`%ProgramFiles%\Git\bin\sh.exe`), which GitHub's images have, and
fails saying so without one. There is no Docker on these runners: Windows
images cannot run Linux containers.

## Run from a checkout

`./setup` builds this checkout and runs `user-memory setup` with it, as the
installer does with a release (it needs the toolchain above):

```bash
./setup
./setup --url https://memory.example.com/mcp --token-file ~/.config/user-memory/client-token
```

Without options it keeps the memories in a local store on this machine,
the installer's native choice; each agent session runs its own
`user-memory serve` against it. For semantic search, put the
`USER_MEMORY_EMBED_*` settings in the checkout's `.env` (start from
`.env.example`), for example
`USER_MEMORY_EMBED_URL=http://127.0.0.1:11434/v1/embeddings`, before running
it ([configuration](../reference.md#configuration)). With `--url`, it
connects this machine's agents to that server instead.

### A server built from the checkout

The repository-root `compose.yaml` builds the image from source and listens
on loopback only. It is the same Docker project as the installer's server
(`name: user-memory`), on the same volume (`user-memory-data`), so the two
serve the same memories, and its container is `user-memory-memory-1` whatever
the checkout's folder is called. They are one server, never two at once:
`docker compose up` in either folder replaces the container the other
started, and `docker compose ps`, `stop` or `down` in either acts on
whichever runs. A new installer server refuses to start while any container
uses the volume.

1. In `.env`, keep the listener on loopback and, on a Mac, point embeddings
   at the Mac's own Ollama (Docker Desktop's `host.docker.internal` reaches
   it; plain http is acceptable because the traffic never leaves the
   machine):

   ```bash
   MEMORY_BIND_IP=127.0.0.1
   MEMORY_HOST=localhost
   USER_MEMORY_EMBED_URL=http://host.docker.internal:11434/v1/embeddings
   USER_MEMORY_EMBED_ALLOW_HTTP=1
   USER_MEMORY_EMBED_MODEL=qwen3-embedding:4b
   ```

   Without a host Ollama, `COMPOSE_PROFILES=embeddings` starts an Ollama
   container instead (CPU-only on macOS), with
   `USER_MEMORY_EMBED_URL=http://ollama:11434/v1/embeddings`; pull the model
   once with `docker compose exec ollama ollama pull qwen3-embedding:4b`.
   After changing `USER_MEMORY_EMBED_*` and recreating the container, check
   the endpoint, calibrate the model and embed every memory with
   `docker compose exec memory user-memory embedding sync`.

2. Start it, copy the token out and connect this machine's agents:

   ```bash
   just local-up
   mkdir -p -m 700 ~/.config/user-memory
   (umask 077; docker compose cp memory:/data/access-token ~/.config/user-memory/client-token)
   ./setup --url http://127.0.0.1:8765/mcp --token-file ~/.config/user-memory/client-token
   ```

   `just local-down` stops it and keeps the volume.

To share it, as [a shared server](../server.md#a-shared-server-behind-https)
describes for the installer's, set `MEMORY_BIND_IP` to the address the proxy
reaches (a private or tunnel address, never `0.0.0.0` on a public
interface) and `MEMORY_HOST` to the host name clients use, then
`docker compose up -d --build --wait`. Without Docker,
`user-memory host --listen ADDR --db /absolute/memories.sqlite3 --token-file
/absolute/access-token --allowed-host memory.example.com` serves the same
endpoint.

To upgrade it after `git pull`, check the store before stopping the server,
so it stays up until the store is known to be ready:

```bash
docker compose build
docker compose run --rm memory migrate --check
docker compose stop memory
docker compose run --rm memory migrate
docker compose up -d --wait memory
```

When the upgrade needs a title for each memory, pass the approved map on
stdin to both, as in `docker compose run --rm -T memory migrate --titles - <
titles.json` ([titles](../reference.md#titles-for-older-memories)). Its
backups, export and import work as for the installer's server
([back up and restore](../server.md#back-up-and-restore)), with plain
`docker compose` in the checkout.

### How the installers set up the Docker server

`install.sh` and `install.ps1` ask the questions and download the builds;
the downloaded `user-memory` writes the compose project, builds and starts
it, upgrades the store and copies the token, through its `server`
subcommand, which is hidden from the help and meant only for the installers.
`uninstall.sh` and `uninstall.ps1` remove the server through it too
(`server remove`):
the container, image, network and project folder go, and the volume with
the memories only when the user confirms deleting them, after an export
unless they decline it.

## Changing the stored schema

Every change to the stored schema (tables, triggers, search-index SQL) bumps
`SCHEMA_VERSION`, adds a snapshot under `tests/fixtures/schema/`, and ships a
tested forward migration step; the build and the schema tests fail otherwise.
The doc comment at `STEPS` in `src/memory/migrate.rs` lists the procedure,
including the rules for table rebuilds.

## Mutation testing

```bash
cargo install cargo-mutants --locked
just mutants                  # Rust code; settings in .cargo/mutants.toml
scripts/sql-mutants --check   # SQL catalog still matches the source (fast)
scripts/sql-mutants           # SQL literals; needs a clean tree
```

`just mutants` runs cargo-mutants with `CARGO_TARGET_DIR` unset (exported,
cargo-mutants builds its mutants into that folder, over your own build) and
three jobs. It builds each mutant with the `mutants` profile in `Cargo.toml`,
the test profile without debug information. Measured on
`src/memory/titles.rs` (45 mutants): 362 s at two jobs, 336 s with the
profile, 317 s with three jobs, and no faster at four. It tests with `cargo
test`, not nextest: nextest lets the slow integration tests still running
finish after a mutant is caught, which made the same run 1,149 s.

cargo-mutants does not mutate string literals, so `scripts/sql-mutants` applies
the exact search/replace pairs in `scripts/sql-mutations/*.txt` (one catalog per
source file) to an export of HEAD and expects `cargo test --locked` to fail.
When SQL changes, entries that no longer match exactly once are reported as
STALE and nothing runs; rewrite or delete them deliberately. Entries marked
`expect: survive` document equivalent mutants. Exit status: 0 all as expected,
1 unexpected result, 2 stale catalog, dirty tree or bad arguments.

When measuring on a clean export (`git archive`), give each export its own
`CARGO_TARGET_DIR`; a reused one can serve a stale binary.

## Agent evaluations

`evals/` runs real Claude Code and Codex sessions against conversation
scenarios, each trial in its own home installed with `user-memory setup`,
grading the stored memories, tool calls, commands and answers. It signs in
with dedicated eval logins you create once, and every session counts against
that plan or API usage; the runner enforces a session budget and aborts if
your real memory, client configuration, instruction files or logins change.
See [evals/README.md](../../evals/README.md).

## Repository rules

- Keep implementation plans, designs and progress logs outside the repository
  (for example in a temporary folder). The commit guard enforces it:

  ```bash
  git config core.hooksPath .githooks
  ```

- Commit maintained setup, operation, configuration and reference
  documentation.
- `AGENTS.md` holds the agent-agnostic repository guidance.

## References

- MCP Rust SDK: https://github.com/modelcontextprotocol/rust-sdk
- Codex MCP: https://learn.chatgpt.com/docs/extend/mcp?surface=app
- Claude MCP: https://code.claude.com/docs/en/mcp
