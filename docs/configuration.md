# Configuration

Setup's and the server's options, the environment variables, the saved
settings and the embedding setting. For the installer's options, see
[install without questions](install.md#install-without-questions).

- [Setup options](#setup-options)
- [Server options](#server-options)
- [Environment](#environment)
- [Saved settings](#saved-settings)
- [Embedding settings](#embedding-settings)
- [Release files](#release-files)

## Setup options

| Option | Effect |
|---|---|
| `--url URL --token-file PATH` | Connect to a remote memory server instead of a local store |
| `--db PATH` | Local store path (default `~/.local/share/user-memory/memories.sqlite3`; `%LOCALAPPDATA%\user-memory\memories.sqlite3` on Windows) |
| `--client codex\|claude\|both` | Which clients to connect (default: every installed one) |
| `--no-hooks` / `--hooks` | Remove or restore both session hooks; remembered |
| `--no-skill` / `--skill` | Remove or restore the skill; remembered |
| `--no-instructions` / `--instructions` | Remove or restore the rules in the clients' instruction files; remembered |
| `--no-update-check` / `--update-check` | Stop or restart doctor's check for a newer release; remembered |
| `--server-only` | With `--url`: a host that runs that server and no agent app; no client is connected and no hooks, skill, instructions or git ignore lines are written. Remembered until `--client` or another server's `--url` is given; doctor then checks only the server side |
| `--env-file PATH` | Read `USER_MEMORY_EMBED_*` from PATH; a checkout's `./setup` passes its `.env` ([run from a checkout](dev/development.md#run-from-a-checkout)) |
| `--install-dir DIR` | Install the binary elsewhere (default `~/.local/bin`; `%LOCALAPPDATA%\Programs\user-memory\bin` on Windows) |

## Server options

| Option | Effect |
|---|---|
| `--listen ADDR` | HTTP bind address (default `127.0.0.1:8765`); put a TLS proxy in front for off-machine access |
| `--db PATH` | Absolute store path |
| `--token-file PATH` | Access-token file; a private random token is created if absent |
| `--allowed-host NAME` | Hostname accepted through a reverse proxy (repeatable) |

Only `/mcp` needs the token; `GET /` answers one line without reading the
store, for health checks.

## Environment

| Variable | Meaning |
|---|---|
| `USER_MEMORY_DB` | Explicit local or server store |
| `USER_MEMORY_URL`, `USER_MEMORY_TOKEN_FILE` | Explicit remote connection |
| `USER_MEMORY_EMBED_URL` | Full embeddings endpoint URL; HTTPS unless loopback; empty means keyword-only. `embedding set --url` also takes a host (`/v1/embeddings` is added) or a base URL (`/embeddings` is added) |
| `USER_MEMORY_EMBED_MODEL` | Model name, default `qwen3-embedding:4b` |
| `USER_MEMORY_EMBED_DIMENSIONS` | Optional shorter vector length |
| `USER_MEMORY_EMBED_QUERY_INSTRUCTION` | Optional prefix for queries only; `\n` is a newline |
| `USER_MEMORY_EMBED_API_KEY_FILE` | Absolute path of a bearer-token file for an authenticated endpoint; never sent over plain http |
| `USER_MEMORY_EMBED_ALLOW_HTTP` | `1` permits plain http to a non-loopback host, for a private network you control |
| `USER_MEMORY_APP_DIRS` | macOS: PATH-style list of directories searched for `ChatGPT.app`/`Codex.app` when `codex` is not on PATH; empty disables the search |
| `USER_MEMORY_NO_UPDATE_CHECK` | `1` stops doctor's check for a newer release for that run |

## Saved settings

`.user-memory.json` beside the installed binary remembers the store or
endpoint, the clients, the hook, skill, instruction-file, update-check and
server-only choices and, for local installs, the embedding settings. Every
local entry point reads the environment first, then these saved settings.
Remote installs save none: the server owns embedding.

## Embedding settings

`user-memory embedding set` changes the setting in one step: it embeds one
probe text (refusing an endpoint that errors or returns no vector),
calibrates the model if it has no built-in cutoffs, saves the setting where
the store lives and re-embeds every memory, since vectors from different
models cannot be compared. For `qwen3-embedding:4b` it also sets 1,024
dimensions and the query instruction the model expects, unless given. On a
local install it saves to `.user-memory.json`; with the installer's Docker
server it rewrites the server's `embedding.env`, recreates the container and
lets the server calibrate and re-embed its own store, keeping the previous
setting if that fails. A client of any other server is refused: the server
owns the setting. `embedding off` saves keyword-only search and drops the
stored vectors.

The cutoffs that decide when two memories may say the same thing
([duplicates](memory-model.md#duplicates-and-corrections)) depend on the
model. `qwen3-embedding:4b` at 2,560 and 1,024 dimensions has cutoffs chosen
by hand, which always apply. Any other model, or another vector length, is
calibrated by `user-memory embedding set`: it embeds the labelled memory
pairs that ship in the binary and sets the duplicate cutoff midway between
the highest related or unrelated pair and the lowest restatement, and the
related cutoff 0.01 below the lowest exception or related pair, then checks
every pair, short and long, with a 0.01 margin. The result is stored in the
store (`embedding_cutoffs`), so a copied store keeps it. A model that cannot
separate the pairs is recorded as such and duplicate detection stays keyword
only; search still uses the model.

## Release files

Releases are published on
[l7aromeo/user-memory](https://github.com/l7aromeo/user-memory/releases) for
six targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`. Each release holds
`user-memory-<target>` (`.exe` on Windows) with its `.sha256`, the
installers `install.sh` and `install.ps1`, the uninstallers `uninstall.sh`
and `uninstall.ps1`, and `THIRD_PARTY_NOTICES.txt`.
[Releasing](dev/releasing.md) says how one is made.
