# Development

Working on user-memory itself. Installing and running it is covered by
[install](../install.md) and [running the server](../server.md); what every
change needs is in [contributing](../../CONTRIBUTING.md), and how a release
is made in [releasing](releasing.md).

## Build and check

Requirements: [rustup](https://rustup.rs/) and a C build toolchain (SQLite
is bundled). The Rust version comes from `rust-toolchain.toml`: in a
checkout, every cargo command uses it with rustfmt and clippy, whatever the
machine's default, and rustup installs it on first use. It is the version the
workflows and the release builds use, the Dockerfile's `RUST_IMAGE`, so
`just check` here and in CI run the same lints. Moving to a new Rust release
is one change in three places, the Dockerfile, `rust-toolchain.toml` and the
workflows' `toolchain:` lines, which `cargo test --test ci` holds together.
Move it when a new stable Rust is out, not only at a release, so the checks
run the lints a release will build with; the release run refuses a Rust
older than the current stable
([releasing](releasing.md#build-and-publish-on-github)).
A build that runs neither rustfmt nor clippy (the server image, the release
builds) names the version itself, through `RUSTUP_TOOLCHAIN` or its image,
so rustup fetches nothing more.

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
ends any test still running after 5 minutes as a failure. The tests that
measure time run alone in every profile, with no other test beside them,
and print their times even when they pass; the override in the same file
lists them. Every workflow job
also has a `timeout-minutes`, about twice its longest green run and at
least 5 minutes; the workflow files give the numbers. The crate has no
doctests: it is a binary without a library, and rustdoc tests only
libraries.

`just check-release` runs clippy again in the release profile, the build
users get. Code compiled only in debug builds, the seams the tests steer
it through (`USER_MEMORY_TEST_VERSION`, the relay, the pauses and stalls),
can leave a variable, import or function unused there, which the debug
lints cannot see. CI runs it after `just check` on Linux, macOS and Windows; `just check`
leaves it out because it checks every crate again. Run it before a change
that adds or moves a `#[cfg(debug_assertions)]` line.

The tests drive the real binary with throwaway stores and fake clients; none
touch your real `~/.claude`, `~/.codex`, skills folders or memory store.

After moving or renaming a checkout or a worktree, run `cargo clean -p
user-memory` before the tests. Each test binary holds the path of the
`user-memory` it runs (`CARGO_BIN_EXE_user-memory`), and cargo does not
rebuild them for a move alone, so they keep running the binary at the old
path, or fail with "No such file or directory" when it is gone.

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
- `tests/support/session.rs` keeps a `user-memory serve` running across
  tool calls, as an agent session does, for tests of what the server does
  between and after calls; `serve.rs` makes one call.

- `tests/acceptance.rs` — end-to-end user scenarios.
- `tests/forgotten_text.rs` — text a forget, a correction or an import
  removed leaves the store file, its log, the keyword index and later
  backups, checked in the bytes on disk; and `migrate` rewrites a store
  that still holds text forgotten before.
- `tests/backups.rs` — a server's own backup checks, through `serve` and
  `host`: at start, once the store is quiet, and as it stops. They shorten
  the quiet hour and the hourly check with `USER_MEMORY_BACKUP_QUIET_MS` and
  `USER_MEMORY_BACKUP_CHECK_MS`, in milliseconds. Those variables are for
  tests only: debug builds read them, and a release build always uses the
  constants in `src/memory/backups.rs`.
- `tests/fixtures/server/` — the `compose.yaml` and `Dockerfile` the shell
  installer wrote before `src/server.rs` took the work over; the module's
  unit tests require the same bytes, so an installed server never changes
  unnoticed.
- `tests/schema.rs` and `tests/fixtures/schema/` — upgrade guards: the current
  schema must match its committed snapshot, and a store built from every
  earlier snapshot must migrate to exactly the current schema while keeping
  all of its rows.
- `tests/mcp.rs` — the MCP server as a client meets it, and the size of what
  a session loads: the server instructions within `INSTRUCTIONS_LIMIT`
  (checked in `src/main.rs`), the tool descriptions within
  `DESCRIPTIONS_LIMIT`, and the whole `tools/list` reply within
  `TOOLS_LIST_LIMIT`. Each limit's comment gives the measurement behind it.
  The tools' fields, types, defaults and values must match
  `tests/fixtures/tools/shape.json`, the reply with its descriptions
  removed: a reworded doc keeps it, and a meant change copies the shape the
  failing test writes.
- `tests/docs.rs` — the docs, the skill and the newest release note against
  the binary: every user-memory command they show runs as written, and every
  message they quote is one the code prints. A quote in double backticks may
  wrap within its paragraph; one left open there fails. A quote may stop
  inside a value the code fills in only after one word of it, and just
  before a value, at its `(`; to leave out more, it ends with `…`. Older
  release notes are history: each was checked when its release was built
  and stays as written.
- `tests/skill.rs` — every tool, parameter, value and result field the skill
  mentions exists in the binary; `tests/docs.rs` checks its commands, quoted
  messages and links with the docs'.
- `tests/docker_context.rs` — every file the release binary embeds is inside
  the Docker build context.
- `tests/windows_install.rs` — `install.ps1` and `uninstall.ps1` under
  PowerShell 7 (`pwsh`): they parse, PSScriptAnalyzer's default rules find
  nothing (each suppression sits in the script with its reason), and help,
  refusals, the build choice, the checksum and the uninstaller's stand-in
  binary behave as on Windows. Without `pwsh` or the PSScriptAnalyzer
  module the tests skip with a note; in CI, or with `REQUIRE_PWSH=1`, they
  fail instead. When PowerShell fails, the failure names its exit code, or
  on Unix the signal that ended it, and what it printed. The check workflow installs PSScriptAnalyzer at the version
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

### macOS: a fresh program killed under load

Under heavy load, a test can fail now and then because a program started
from a path macOS has not seen before is killed with SIGKILL before it runs
a line: the rebuilt `target/debug/user-memory`, so setup fails with empty
output, or a test's freshly linked fake client, so setup reports `ended with
signal: 9 (SIGKILL)`. AppleSystemPolicy, the Gatekeeper check on a new
executable, waits for its policy daemon; when that wait is interrupted it
refuses the program. The kernel log shows, at the failure:

```text
ASP: Sleep interrupted: ref N, signal 0x100, pid: P
ASP: Security policy would not allow process: P, <path>
```

To confirm one, keep this running while the tests run, and read the lines
around the failure. Use the full path: in zsh, `log` is a builtin.

```bash
/usr/bin/log stream --style compact --predicate 'process == "kernel" OR process == "amfid" OR eventMessage CONTAINS "memorystatus"'
```

AMFI's `code signature validation failed` lines are noise: every locally
built program gets them, passing or not. The tests do not retry: a retry
would hide real failures of setup and the clients it runs, and the next run
passes.

### Windows

The `windows` workflow runs `just check` on Windows x64 (`windows-latest`) and
ARM64 (`windows-11-arm`), on every push to main and on pull requests that
change code, tests, the build, scripts, workflows or the PowerShell
installers; a docs-only change skips it. There the recipes run in Windows
PowerShell (the justfile's `windows-shell`), the install.ps1 tests are
required (`REQUIRE_PWSH`), and x64 uses aws-lc-sys's prebuilt NASM, which
the image lacks. After `just check` and `just check-release`, the install.ps1 tests run again under
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
it ([configuration](../configuration.md#environment)). The file holds
`KEY=VALUE` lines, comments and optional quotes, with no `$VAR` expansion;
an unknown `USER_MEMORY_EMBED_*` name is an error, and every other key is
ignored. `./setup --env-file FILE` reads another file. With `--url`, it
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

Before 0.7.0 the file named no project, so Compose named it after the
checkout's folder (`<folder>-memory-1`). To move such a server to the pinned
name, stop it by its old project name first: a plain `docker compose down`
now looks for `user-memory` and leaves the old container running.

```bash
docker compose -p <folder> down
docker compose up -d
```

Never add `-v`, which deletes the memories. From then on `up` warns that the
volume "already exists but was created for project "<folder>" (expected
"user-memory")". That is harmless: the volume keeps its memories and is
reused as it is. The installer's server never sees this warning: every
installer has written its project's name into its compose file and created
the volume through that project.

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
titles.json` ([titles](../storage.md#titles-for-older-memories)). Its
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

## Work without touching your memories

The tests, the evals and `just retrieval-bench` each build a throwaway store
and home. A build you run by hand does not: it finds its store, settings and
state from your environment, so it works on your own memories unless you
point it elsewhere.

### Run a checkout build against a throwaway store

Build first, with your own home, so cargo finds its toolchain, and make a
folder only you can read for the throwaway store and home:

```bash
cargo build
```

```bash
dev=$(mktemp -d)
```

Then run the binary in a subshell whose store, home and clients' folders
all point into that folder, so nothing leaks into your shell:

```bash
(
  export HOME="$dev" XDG_DATA_HOME="$dev/.local/share" XDG_STATE_HOME="$dev/.local/state" XDG_CONFIG_HOME="$dev/.config"
  export CLAUDE_CONFIG_DIR="$dev/.claude" CODEX_HOME="$dev/.codex" GIT_CONFIG_GLOBAL="$dev/.gitconfig"
  export USER_MEMORY_DB="$dev/memories.sqlite3" USER_MEMORY_NO_UPDATE_CHECK=1
  unset USER_MEMORY_URL USER_MEMORY_TOKEN_FILE USER_MEMORY_DOCKER_NAME
  ./target/debug/user-memory index
  echo "throwaway store in $dev"
)
```

These are the variables the binary reads to find files, the same ones the
tests' sandbox sets (`tests/support/sandbox.rs`). On Windows, set
`USERPROFILE`, `LOCALAPPDATA` and `APPDATA` in place of `HOME` and the XDG
folders.

What each one keeps away from your own files:

| Without | A build run by hand |
|---|---|
| `USER_MEMORY_DB` | Opens the default store, `~/.local/share/user-memory/memories.sqlite3`: your real store on a native install, or a new one there when you use a server. It never reads the installed setup, which sits beside the installed binary, so it does not use your server either. |
| `HOME` and `XDG_STATE_HOME` | `session-start` and `prompt-submit` save snapshots in your `~/.local/state/user-memory/sessions/`, even with `USER_MEMORY_DB` set. Each is named by the store it saw, so it never replaces your sessions' snapshots, but it stays there until the 30-day clean-up. |
| `HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` | `setup` and `uninstall` change your clients' settings, hooks, instruction files and skill, and install or remove the binary and its saved setup in `~/.local/bin`. |
| `HOME` and `GIT_CONFIG_GLOBAL` | `setup` and `project-id --set` or `--include` add lines to the global git ignore your git configuration names. |
| `USER_MEMORY_NO_UPDATE_CHECK=1` | `doctor` asks GitHub for the newest release. |

With `USER_MEMORY_DB` set and nothing else, a dev build leaves your store,
your instruction files and your setup alone: it has no saved setup of its
own, so it syncs no instruction file. Only the session snapshots above land
in your home, under names of the build's store. Two more files are the build's own: automatic backups go in a
`backups` folder beside the throwaway store, and `embedding set` or
`embedding off` saves its choice in `target/debug/.user-memory.json`, which
later runs of that build read. Embeddings stay off unless you set
`USER_MEMORY_EMBED_URL` ([configuration](../configuration.md#environment)).

### Measure on a copy of your store

To try a change on real data, work on a private copy, never on the live
store. Reading the live store changes it too: with semantic search on, a
search saves vectors for the memories that lack one, and a session start
saves a snapshot. Keep the copy in a folder only you can read (`mktemp -d`
makes one), outside any repository, and delete it when you are done.

For a native store, the newest [automatic backup](../backup.md#automatic-backups)
is already a whole, consistent copy, and copying it touches nothing:

```bash
cp ~/.local/share/user-memory/backups/memories-<date>.sqlite3 "$dev/memories.sqlite3"
```

For an up-to-the-minute copy, SQLite's own backup reads the store with its
write-ahead log, while your agents keep using it:

```bash
sqlite3 ~/.local/share/user-memory/memories.sqlite3 ".backup '$dev/memories.sqlite3'"
```

Never copy `memories.sqlite3` alone while it is in use: recent writes sit in
`memories.sqlite3-wal` beside it, so a copy without them is out of date, or
caught in the middle of a write.

For the installer's Docker server, copy its newest automatic backup out of
the volume, or export through the container:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml cp memory:/data/backups/memories-<date>.sqlite3 "$dev/memories.sqlite3"
```

```bash
(umask 077; docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec -T memory user-memory export - > "$dev/memories.json")
```

An export imports into the throwaway store with
`./target/debug/user-memory import "$dev/memories.json" --apply`, run in a
subshell set up as above. It leaves out conversation links and vectors, so a
measurement that needs them uses a store copy. `user-memory export` works the
same way from any machine connected to your server.

Whatever you measure, what you write down holds only synthetic memories,
ids and numbers: fixtures, benchmarks, test data, issues and pull requests
never quote a real memory or its wording. The retrieval benchmark's
`evals/retrieval/memories.json` shows the kind of synthetic memory to use.

### Clean up

Delete the throwaway folder, which holds the store, its backups, any copy
and the session snapshots of the build:

```bash
rm -rf "$dev"
```

If you ran `embedding set` with the dev build, also delete
`target/debug/.user-memory.json`, so the next run starts with no saved
setting.

## Example ids and titles

Examples are plainly made up and the same few everywhere: the docs, the
skill, help texts, tests, fixtures and evals use these memories.

| Id | Short | Title |
|---|---|---|
| `e0000001-0000-4000-8000-000000000001` | `e0000001` | Ask before every git push |
| `e0000002-0000-4000-8000-000000000002` | `e0000002` | Use Python 3.13 in this repository |
| `e0000003-0000-4000-8000-000000000003` | `e0000003` | Ship the parser by November |
| `e0000004-0000-4000-8000-000000000004` | `e0000004` | Answer in short paragraphs |
| `e0000005-0000-4000-8000-000000000005` | `e0000005` | Run the tests before every commit |
| `e0000006-0000-4000-8000-000000000006` | `e0000006` | Indent with tabs |

A further example continues the count: `e0000007-0000-4000-8000-000000000007`.
The docs, the skill and help texts use only these titles; the content is
the title unless the passage needs more. Tests may use other titles too,
as long as they are plainly generic and describe nobody in particular.

Two other families stay where they are:

- `1a2b3c4d-1111-4aaa-8aaa-000000000001` and on: the schema snapshots'
  seeds in `tests/fixtures/schema/` and the tests that read them.
- `00000000-0000-4000-8000-000000000001` and on, up to `…0000ff`: ids a
  test needs and never shows.

A test that needs an id of a particular shape uses one of these, each with
the reason beside it in `SHAPED_IDS` in `tests/docs.rs`:

| Id | Why |
|---|---|
| `6ba7b810-9dad-11d1-80b4-00c04fd430c8` | Version 1 (RFC 9562's DNS namespace), which an import refuses |
| `6ba7b810-9dad-41d1-c0b4-00c04fd430c8` | Version 4 with the Microsoft variant, refused too |
| `00000000-0000-1000-8000-000000000003` | Not version 4 |
| `00000000-0000-4000-c000-000000000004` | Version 4 with the wrong variant |
| `abcdef12-0000-4000-8000-000000000001`, `…002` | Two ids sharing their short form |
| `11111111-1111-4111-8111-111111111111` | Another memory, named only in a field that refers to one |
| `1a2b3c4d-0000-4000-8000-00000000dead` | An index entry left for a memory that is gone |
| `ffffffff-ffff-4fff-bfff-ffffffffffff`, `ffffffff-0000-4000-8000-ffffffffffff` | Ids that sort after every other |

`every_uuid_is_an_example` in `tests/docs.rs` fails on any other UUID in
any file git tracks that reads as text, scripts and settings included, and
on any other short id in the docs and the skill:
8 hex characters with a letter among them, standing alone or cut short as
`e0000001-…`. A hash or commit id that is not a memory id is written so it
cannot be taken for one: a commit id at 7 characters or at least 9, and a
hash joined to its name by a dot, as an import's name ends in `.e0e0e0e0`.
Store ids are 32 hex characters without hyphens, and session ids are
written `<id>`, so neither looks like a memory id.

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
three jobs; when you run `cargo mutants` yourself, unset it the same way. It
builds each mutant with the `mutants` profile in `Cargo.toml`, the test
profile without debug information. Measured on `src/memory/titles.rs` (45
mutants): 362 s at two jobs, 336 s with the profile, 317 s with three jobs,
and no faster at four. It tests with `cargo test`, not nextest: nextest lets
the slow integration tests still running finish after a mutant is caught,
which made the same run 1,149 s.

cargo-mutants does not mutate string literals, so `scripts/sql-mutants` applies
the exact search/replace pairs in `scripts/sql-mutations/*.txt` (one catalog per
source file) to an export of HEAD and expects `cargo test --locked` to fail.
When SQL changes, entries that no longer match exactly once are reported as
STALE and nothing runs; rewrite or delete them deliberately. Entries marked
`expect: survive` document equivalent mutants. Exit status: 0 all as expected,
1 unexpected result, 2 stale catalog, dirty tree or bad arguments.

When measuring on a clean export (`git archive`), extract it with `tar -x -m`
or give each export its own `CARGO_TARGET_DIR`. `git archive` stamps files
with the commit's time, so a reused target folder whose last build is newer
takes the commit as built already and serves the previous binary. The
evaluation harness prints each binary's SHA-256 and refuses to compare two
with the same contents ([evals/README.md](../../evals/README.md#running-a-batch)).

## Agent evaluations

`evals/` runs real Claude Code and Codex sessions against conversation
scenarios, each trial in its own home installed with `user-memory setup`,
grading the stored memories, tool calls, commands and answers. It signs in
with dedicated eval logins you create once, and every session counts against
that plan or API usage; the runner enforces a session budget and aborts if
your real memory, client configuration, instruction files or logins change.
The budget's counter and the reports live in a per-user folder a restart
keeps (`~/Library/Caches/user-memory-eval/reports` on macOS, the XDG state
folder elsewhere), and a folder that lost its counter is refused rather than
counted as zero. See [evals/README.md](../../evals/README.md).

A batch runs each provider's cheaper model from its latest family: Claude
Code's `sonnet` alias and Codex's `gpt-6-luna`. A weaker agent makes a pass
stronger evidence that memory carried the decision, and the batch costs less;
the alias follows new families, and the runner stops when Codex reports its
named model superseded. `--top-models` runs `opus` and Codex's top tier, for
re-running a failure before calling it a bug. Each batch's summary reports
the tokens and estimated cost per scenario, client and model, so budgets use
measured figures.

## Retrieval benchmark

`just retrieval-bench` measures how well `search` finds a known memory. It
builds a throwaway store from the synthetic memories in
`evals/retrieval/memories.json`, then runs the graded queries in
`evals/retrieval/queries.json` in the session's project. There are three
styles of query: sharing words with the memory, phrased as a task, and
paraphrased. Per style it reports top-1, top-3, recall at 5, mean
reciprocal rank (MRR), NDCG at 10 over each query's grades (2 for its
target, 1 for the memories it names as related), misses, and the size of
the default page as an agent receives it. It also reports the ranks of
global targets among global memories alone. It runs keyword-only, and with
`--embed-url` also fused with an embeddings endpoint; the qwen3-embedding
defaults match the installer's:

    just retrieval-bench --embed-url http://127.0.0.1:11434/v1/embeddings

Some queries test a case, and the report counts their wins apart:

- **superseded**: a memory replaced by another (`supersedes` in
  memories.json) is never returned.
- **exception**: a project's exception to a global rule on the same subject
  is within the default page, even when the query's words favour the rule.
- **near-duplicate**: of two memories that share most of their words but
  mean different things, the one the query asks for ranks above the other.

Run it before and after a change to ranking, and report both. It needs no
CI and runs no agent. `--out FILE` writes every query's ranks as JSON, and
`--keep DIR` keeps the built store with its vectors; both are refused inside
the repository. `just eval-tests` runs the checks of the benchmark's own
metrics, cases and private mode, which CI runs too.

### On a private query set

`--memories FILE` and `--queries FILE` replace the fixtures, to measure
search on memories of your own. `--memories` takes a `user-memory export`
file, which loads with `import` so ids, statuses, evidence and
supersessions stay as they are, or a file in the benchmark's own format.
The queries use the benchmark's format; their `target`, `related`, `rival`
and `excluded` name memory ids, whole or as the 8 characters the index
shows.

1. Make a private copy of your store and a throwaway home, as
   [measure on a copy](#measure-on-a-copy-of-your-store) shows, and export
   the copy from a subshell pointed at it:
   `./target/debug/user-memory export "$dev/memories.json"`.
2. Write your queries to `$dev/queries.json`, outside the repository.
3. Run the benchmark on them:

   ```bash
   just retrieval-bench --memories "$dev/memories.json" --queries "$dev/queries.json" --out "$dev/report.json"
   ```

4. [Delete](#clean-up) `$dev` when you are done.

With either file given, nothing the benchmark prints or writes holds a
query's or a memory's text: it names queries by number and memories by id,
in the report, in `--out` and in its errors, so the report can be shared.
Every one of these paths is refused inside the repository. An embeddings
endpoint is accepted only when every address its host resolves to is a
loopback address, since semantic search sends it memory text
([privacy](../privacy.md#what-leaves-your-machine)); any other is refused
before anything is sent.

## Search budget

`just search-budget` checks that search stays fast as the store grows. It
builds a throwaway store of 100,000 synthetic memories (`--memories N` for
another size), builds its keyword index with `reindex`, gives every memory a
vector of 2,560 dimensions (`--dimensions N`), then times searches whose
words few, some and many memories hold, keyword-only and fused, as a whole
command-line process and inside one long-running `serve`, and times
`user-memory embedding show`, which counts the stored vectors. Query
embeddings come from a stand-in endpoint in the script that answers at once.
It prints each median, the server's memory and its first fused search, and
fails when one is over its budget: 150 ms for a keyword search on the
command line, 100 ms for any search on the server, and 500 ms for
`embedding show` (`evals/scale/run.py`). A fused search
on the command line reads every vector and is reported, not checked.
`--keep DIR` keeps the built store and `--store FILE` times it again. Run it
after a change to search, the keyword index or the vectors, and report what
it printed. It takes a few minutes and needs no CI.

## Upgrade and write budgets

Two more scripts build the same synthetic store and time what a schema
change costs. Each needs a release build of an earlier version, made from a
clean export (`git archive`) with its own `CARGO_TARGET_DIR`.

`just upgrade-budget --from-binary OLD` builds 100,000 memories, each with a
vector, with that build, then times `user-memory migrate` with this
checkout's build on a fresh copy each run: the backup, every schema step and
the rewrite, as setup runs them. It checks for one `.bak` and every
memory's content kept byte for byte, prints the store's and the keyword
index's sizes before and after, and fails when the median is over the
budget in `evals/scale/upgrade.py`. A change to the stored schema runs it
from the last release's schema and reports what it printed.

`just write-budget --baseline-binary OLD` times `record`, `update` and
`forget` as command-line processes on 100,000 memories, the median of 14
runs after a warm-up: with that build, with this one, and with this one
without the history's triggers. It ends with the history's size after a
year of synthetic changes.

## Delivery benchmark

`just delivery-bench` measures delivery matching (`src/memory/matcher.rs`,
over the index file of `src/memory/delivery.rs`): what an agent's actions
match among a store's passages, and what the bars push after an action,
push after a failed one, or hold before a command. The tool hooks use the
same modules in the shipped binary. The benchmark reaches them through
`user-memory-bench`, a second binary (`evals/delivery/entry.rs`) that
releases never build, as they never build `user-memory-fake`.

That binary compiles the matcher and the modules it uses from their own
files, through a `#[path]` mirror of `src/memory`: `delivery.rs`,
`matcher.rs`, `words.rs`, `index/passages.rs`, `index/stems.rs` and
`index/weights.rs`. Each says so at its top. A mirrored file that starts
using a module outside the mirror breaks that binary's build, which `just
check` builds with every target; move what it needs into a mirrored file
instead.

For each store size and seed, `evals/delivery/run.py` draws a synthetic
world from `evals/delivery/world.py`: a project's commands, files and
components; notes, traps and lessons about them, with look-alikes about
something else; busy commands with several notes each; rules; a project's
essentials; long merged memories; filler from `evals/scale`'s vocabulary;
and whole tasks of five kinds of work, each action with the memories it
needs then. A stated share of actions spell their command or path another
way (an alias, a wrapper, a flag before the subcommand, a path from the
root, a script run by `./` or through its interpreter), run their command
in a line with others (a chain or a pipe), or read their file in one shell
command beside other files. It writes the memories into a throwaway store,
indexes them with `user-memory reindex`, builds the delivery index, runs
every task through `user-memory-bench match` and reports what the bars
deliver, at most three pushes an action (`evals/delivery/metrics.py`):

- precision three ways: needed at that moment, about the action, about
  something else; and by when each delivery was seen against its
  command's run;
- recall over all needs and over needs whose point an anchor can match,
  by holders of the anchor and by how the action spells it, with timing;
- script runs by how they are invoked (bare, `./`, `bash` or `sh`,
  `python3`, with flags), each with its precision and recall;
- holds, the commands they deny per task and per session, and failures;
- why each missed need was missed: its memory holds no anchor, its anchor
  matched nothing, or it matched below the bar.

Commands:

    just delivery-bench                  # 50, 1,000 and 10,000 memories
    just delivery-bench --sizes 50       # one size
    just delivery-bench --sweep          # bars and window, on the tuning seeds
    just delivery-bench --cost           # one lookup's time, 10,000 memories
    just delivery-bench --check-cost     # the same, failing past a budget

`--cost` times one lookup from a new process, and the build of the index
from the store, which it reports. Through the release binary, in the same
throwaway home, it also times the delivery index's upkeep: session start
building its scope's base against session start finding it (budget: the
difference is at most 75 % of the build's own time, since the build runs
beside session start's own index and one that finishes first adds little
of itself; each of 21 rounds times the build alone and the two session
starts one after the other, in an order that rotates from round to round,
and the check takes the median of the rounds' shares, so a runner that
slows down between rounds moves none of them); a tool hook that finds a
change, process start included, once pushing nothing and once pushing the
memory its command names (budget: `TOOL_HOOK_LIMIT`); the tool hook in a
session whose project waits for a tag, in a folder the session never saw,
pushing the memory its command names, then the tag check (budget: two
`TOOL_HOOK_LIMIT`s, the push's and the check's own after it); the prompt
hook pushing the memory its words name (budget: `PROMPT_LIMIT`); what a
forget adds when three scopes' files hold the memory (budget: 400 ms);
whether the prompt hook builds a missing base within `PROMPT_LIMIT`; and,
with the store served by `host` on loopback to a client in a throwaway
home, session start downloading its base against finding it (budget:
session start's `LIMIT`), the base's size, and how long a forget on the
server takes to leave the client's files with its connector running
(budget: three of `REMOTE_MARKER_POLL`).
`--check-cost` fails when any is past its budget. CI runs it on macOS,
Linux and Windows.

The bars and window are set on the tuning seeds (`--sweep`) and reported on
others, so the report isn't graded on what set them. The report uses the
matcher's own bars: a command's passages on their share of what the
command could score, every other action's on a score where an anchor's
worth falls with the square root of the memories holding it, and every
push needing an anchor of the action. The sweep tries single bars on the
store's score, on the share and on the mixed score. `--bars` reports at
other bars, on any of those with `scale=NAME` (repeat it to compare several
on the same stores), `--prompt-curve` at several prompt bars on stems alone,
`--failure-curve` at several failure bars with and without an anchor, and
`--keep DIR` keeps the built stores. `--presence` adds how often session
start already lists what the tasks need: today's, from `user-memory
session-start`, and a model of the index ordered by use, whose constants
are in `evals/delivery/presence.py`. The measures' own tests run with
`python3 -I evals/delivery/test_bench.py`.

## Repository rules

- Keep implementation plans, designs and progress logs outside the repository
  (for example in a temporary folder). The commit guard enforces it:

  ```bash
  git config core.hooksPath .githooks
  ```

- Commit maintained setup, operation, configuration and reference
  documentation.
- [AGENTS.md](../../AGENTS.md) holds the rules every change is held to, for
  people and tools alike.

## References

- MCP Rust SDK: https://github.com/modelcontextprotocol/rust-sdk
- Codex MCP: https://learn.chatgpt.com/docs/extend/mcp?surface=app
- Claude MCP: https://code.claude.com/docs/en/mcp
