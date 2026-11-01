# Install, upgrade and uninstall

user-memory runs on macOS and Linux (Apple Silicon, Intel or ARM) and, as a
preview, on Windows 10 and 11 (x64 or ARM64). It works with Claude Code and Codex; setup
connects every one it finds on the machine. On macOS it also finds Codex
inside the ChatGPT app.

- [Install](#install)
- [Install on Windows](#install-on-windows)
- [Choices the installer offers](#choices-the-installer-offers)
- [Install without questions](#install-without-questions)
- [Approve the hooks in Codex](#approve-the-hooks-in-codex)
- [Check that it works](#check-that-it-works)
- [What setup changes](#what-setup-changes)
- [More machines](#more-machines)
- [Upgrade](#upgrade)
- [Turn parts off](#turn-parts-off)
- [Uninstall](#uninstall)

## Install

Run this on each machine where you use Claude Code or Codex:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

To join an existing server you need its address, such as
`https://memory.example.com`, and its access token
([where it comes from](server.md#the-access-token)). Treat the token like a
password: anyone with it can read and change your memories. Without a
server, the installer can set one up on this machine.

The installer asks a few questions. Enter takes the default, shown in
capitals:

- ``Do you already have a user-memory server?`` [y/N]
  - **y:** it asks for the
    ``Memory server address (e.g. https://memory.example.com or http://127.0.0.1:8765):``,
    then ``Access token for <endpoint> (input hidden):``.
  - **N:** ``Set up a memory server on this machine?`` [Y/n]
    - **Y:** ``Run it in Docker?`` [Y/n]. Without Docker it asks
      ``Keep the memories in a local store on this machine instead?`` [Y/n]
      instead.
    - **n:** nothing is installed.

Then it:

1. downloads the client for this machine from the same release and checks its
   SHA-256 (a mismatch installs nothing);
2. installs it to `~/.local/bin/user-memory`;
3. runs `user-memory setup`, which connects your agents;
4. runs `user-memory doctor`, which checks everything.

The installer never prints the access token. It saves it to
`~/.config/user-memory/client-token`, readable only by you.

## Install on Windows

Windows support is a preview: it is tested in CI on x64 and ARM64, not yet
with Claude Code and Codex on a real Windows machine. Please report what you
find in an [issue](https://github.com/l7aromeo/user-memory/issues).

In PowerShell (Windows PowerShell 5.1 or PowerShell 7), as yourself, not as
administrator:

```powershell
irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1 | iex
```

It asks the same questions as on macOS and Linux. To give options, run it
as a script block. Each option is the shell installer's, written as a
PowerShell parameter: `--local docker` is `-Local docker`, `--allowed-host`
is `-AllowedHost`, `--no-embed` is `-NoEmbed`, and `-Help` lists them all.

```powershell
& ([scriptblock]::Create((irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1))) -Local docker
```

| Where | What |
|---|---|
| `%LOCALAPPDATA%\Programs\user-memory\bin\user-memory.exe` | The client binary. Setup adds its folder to your user PATH; open a new terminal to run `user-memory`. |
| `%LOCALAPPDATA%\user-memory\memories.sqlite3` | The local store, readable only by you. |
| `%LOCALAPPDATA%\user-memory\config\client-token` | The access token, readable only by you. |
| `%LOCALAPPDATA%\user-memory\docker\` | The Docker Desktop server's project, with `-Local docker`. |

- The binary is not code-signed yet. The installer checks its SHA-256
  against the release before it runs it, and says so when Smart App Control
  or an antivirus blocks it. To check a download yourself, compare
  `(Get-FileHash user-memory-x86_64-pc-windows-msvc.exe).Hash` with the
  release's `user-memory-x86_64-pc-windows-msvc.exe.sha256` (on ARM64,
  `aarch64-pc-windows-msvc`).
- Claude Code runs the hooks without a shell, which needs Claude Code
  2.1.139 or newer: run `claude update` first. Setup and `doctor` say so
  when it is older. Codex runs them through
  `cmd.exe`.
- The skill is linked where Windows allows symbolic links (Developer Mode),
  and copied otherwise.
- Codex inside the ChatGPT app is not found on Windows; install the Codex
  CLI.

## Choices the installer offers

| Choice | What you get |
|---|---|
| Existing server | This machine's agents use that server's memories. The client holds none itself: every call goes to the server, with your access token. |
| Docker (default for a new server) | A server container on `127.0.0.1:8765`. Memories live in the Docker volume `user-memory-data`. |
| Native | A store at `~/.local/share/user-memory/memories.sqlite3`. Each agent session starts its own server; nothing runs in the background. |
| Server only (`--server-only`) | The Docker server, for a host with no agent app: no agent is connected there, and other machines use it through an HTTPS proxy ([a shared server](server.md#a-shared-server-behind-https)). |

A server on this machine (Docker or native) is not shared with your other
machines. To share one memory, see
[a shared server](server.md#a-shared-server-behind-https).

### Semantic search

For a server on this machine, the installer then asks how to find memories
by meaning as well as by their words:

| Choice | What you get |
|---|---|
| Ollama on this machine (default) | [Ollama](https://ollama.com) computes the embeddings here; nothing leaves the machine and no API key is needed. The installer lists the embedding models Ollama already has, with `qwen3-embedding:4b` recommended. |
| Another endpoint | Any OpenAI-compatible embeddings endpoint: its address (`/v1/embeddings` is added to a bare host, `/embeddings` to another path), the model, and an API key, typed hidden and saved to `~/.config/user-memory/embedding-key` (mode 0600). |
| Off | Search matches keywords only. |

The installer installs or downloads nothing without asking first. When
Ollama is missing it asks ``Ollama isn't installed. Install it now? [y/N]``:
on a Mac it uses Homebrew (`brew install ollama`, then starts it) or, without
Homebrew, shows the download page and waits; on Linux it runs Ollama's own
install script, which asks for your password (sudo). When Ollama has no
embedding model it asks ``Download qwen3-embedding:4b (about 2.5 GB)? [Y/n]``.

`qwen3-embedding:4b` is calibrated in advance. For any other model,
user-memory calibrates duplicate detection itself on a set of example memory
pairs it ships with; a model that cannot tell them apart still searches by
meaning, while duplicates are caught by keywords.

Running the installer again keeps the choice, unless you give one of the
options below. `user-memory embedding set --url URL --model NAME` and
`user-memory embedding off` change it too, and `user-memory embedding show`
says what is in use ([semantic search](server.md#semantic-search)).

## Install without questions

From a provisioning script, or without a terminal, give the answers as
options after `sh -s --`:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --url https://memory.example.com --token-file /path/to/token
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --local docker
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --local native
```

| Option | Meaning |
|---|---|
| `--url URL` | Use an existing server. `/mcp` is added when missing. Also read from `USER_MEMORY_URL`. |
| `--local docker` or `--local native` | Set up a server on this machine. |
| `--port N` | Port of the local Docker server (default 8765). |
| `--allowed-host HOST` | With `--local docker`: accept requests for HOST too, for a server shared through an HTTPS proxy (repeatable; see [a shared server](server.md#a-shared-server-behind-https)). |
| `--bind ADDR` | With `--local docker`: also publish the server on this IPv4 address, for a proxy on another machine (default: `127.0.0.1` only). |
| `--token-file FILE` | Read the access token from this file. |
| `--version vX.Y.Z` | Install that release instead of the latest. |
| `--install-dir DIR` | Install the binary to DIR (default `~/.local/bin`). |
| `--client NAME` | `auto` (every client found, the default), `codex`, `claude` or `both`. |
| `--server-only` | Run the Docker server on a host with no agent app, and connect none. Reruns keep it; `--client` or `--url` makes the host a client too. Not with `--url`, `--local native` or `--client`. |
| `--titles FILE` | Titles for an upgrade from 0.3 (see [upgrade](upgrade.md#upgrade-from-03-titles)). |
| `--embed-model NAME` | Semantic search through Ollama on this machine with that model (default `qwen3-embedding:4b`). |
| `--install-ollama` | Install Ollama if it is missing (Homebrew on macOS, Ollama's install script with sudo on Linux). |
| `--pull-model` | Download the model into Ollama if it is missing. |
| `--embed-url URL` | Semantic search through another OpenAI-compatible endpoint instead, with `--embed-model`. |
| `--embed-key-file FILE` | The API key for `--embed-url`, read from FILE whenever it is used. |
| `--embed-dimensions N` | Ask the model for vectors of length N. |
| `--no-embed` | Keyword search only. |

The semantic search options apply to a server on this machine; a shared
server's machine sets its own.

The token comes from `--token-file`, else the `USER_MEMORY_TOKEN` variable,
else the token already saved for that server, else a hidden prompt. The
installer sends a token over plain `http://` only to an address on this
machine. The address and token can come from the environment too:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | USER_MEMORY_URL=https://memory.example.com USER_MEMORY_TOKEN="$TOKEN" sh
```

`USER_MEMORY_RELEASES` replaces the releases address, for a mirror, and
`USER_MEMORY_OLLAMA_URL` the address of Ollama on this machine (plain
`http://` to this machine only).

## Approve the hooks in Codex

Codex runs a new or changed hook only after you approve it once. Start Codex,
run `/hooks`, and choose **Trust** for user-memory's **SessionStart** hook
("Loading user memory") and its **UserPromptSubmit** hook. Claude Code needs
no approval.

Until you approve them, memory does not run in Codex, though nothing is
broken. The installer still finishes, and its last lines name the step:

```output
Still to do before memory works everywhere:
  Codex session hooks: Codex has not approved them yet, so they never run; open Codex and approve them once: at startup Codex says hooks need review, or run /hooks and choose Trust for user-memory's SessionStart and UserPromptSubmit hooks
```

`user-memory doctor` shows the same step until you have done it.

## Check that it works

Restart Claude Code and Codex once, so they start the new connector. Then run:

```bash
user-memory doctor
```

With a server, it prints:

```output
Binary: OK
Remote MCP connection and tools: OK
Embedding: managed by the memory server
Skill: OK
Claude Code instructions (~/.claude/rules/user-memory.md): current
Codex instructions (~/.codex/AGENTS.md): current
Git ignore: OK (~/.config/git/ignore ignores .user-memory and .user-memory-key)
Codex: OK
Codex session hook: OK (approved in Codex)
Claude Code: OK
Claude Code session hook: OK
```

It exits with code 0 when all is well, 3 when nothing is broken but a step
is left for you (its `Action needed:` lines, last), and 1 on an error.

doctor also asks GitHub once for the newest release. When it is newer than
the one installed, doctor says so, for example ``user-memory 0.7.0 is
available (this is 0.6.0): run the installer again``. The request carries
nothing about you or your memories, a failed one prints nothing, and the
session hooks never make it. `user-memory setup --no-update-check` turns it
off ([turn parts off](#turn-parts-off)).

Open a **new** session and ask "what does user-memory know about you?". A
resumed session keeps the tools it started with, so it may not see
user-memory yet. The agent should name the conversation origin, the projects
the session loads (or that the folder has none), and the index of your
memories. On a new server that is ``No user memories recorded yet``.

## What setup changes

| Where | What | How |
|---|---|---|
| `~/.local/bin/user-memory` | The client binary. | Replaced in one step. |
| Claude Code and Codex MCP settings | A `user-memory` server entry. | Through each client's own CLI (`claude mcp`, user scope; `codex mcp`). |
| `~/.claude/settings.json`, `~/.codex/hooks.json` | Two hooks each: one at session start, one before each prompt. | Only its own entries; your other settings and their key order stay. |
| `~/.claude/rules/user-memory.md`, and a marked block in `~/.codex/AGENTS.md` | Your global rules (see [how it works](how-it-works.md#your-rules-in-instruction-files)). | Your own text in AGENTS.md stays as it is. |
| `~/.agents/skills/user-memory`, linked from `~/.claude/skills/user-memory` | The user-memory skill. | Never touches a skills manager's lock file. |
| Your global git ignore | The lines `.user-memory` and `.user-memory-key`, so those files are never committed ([projects](projects.md#the-git-ignore)). | Only those lines. |
| `~/.local/bin/.user-memory.json` | What setup chose: server or store, clients, hooks, skill. | Readable only by you. |
| Your user PATH (Windows) | The bin folder, added at the end so your own entries come first. | Recorded, so uninstall removes only that entry. |

Each operating system keeps these in its own place; the files under your
profile (`.claude`, `.codex`, `.agents`) are the same on all of them:

| | macOS and Linux | Windows |
|---|---|---|
| Binary and `.user-memory.json` | `~/.local/bin` | `%LOCALAPPDATA%\Programs\user-memory\bin` |
| Local store | `~/.local/share/user-memory` | `%LOCALAPPDATA%\user-memory` |
| Session snapshots and write records | `$XDG_STATE_HOME` or `~/.local/state`, then `user-memory` | `%LOCALAPPDATA%\user-memory\state` |
| Access token | `$XDG_CONFIG_HOME` or `~/.config`, then `user-memory/client-token` | `%LOCALAPPDATA%\user-memory\config\client-token` |
| Claude Code hooks | A shell command. | The binary and its arguments, run without a shell. |
| Codex hooks | A shell command. | A `cmd.exe` command. |

Private files are mode 0600 in 0700 folders on macOS and Linux, and on
Windows carry an access list that allows only you; doctor checks the store
(``Store privacy: OK``).

Setup prints every file it changes. It refuses a settings file that is not
valid JSON, and changes nothing on a second run. It never edits Codex's
`config.toml` or your `~/.claude/CLAUDE.md`. [Claude Code and
Codex](clients.md) describes the [hooks](clients.md#session-hooks) and
[the skill](clients.md#the-user-memory-skill) in detail.

## More machines

Run the same one-line installer on each machine and answer `y` to "Do you
already have a user-memory server?", with the same address and token. To
switch a machine to another server or a new token, run the installer again
with the new address, or:

```bash
user-memory setup --url https://memory.example.com --token-file ~/.config/user-memory/client-token
```

## Upgrade

Run the installer again. On a machine that is already set up it asks
nothing, and it backs the store up before any change to its format.
[Upgrade](upgrade.md) covers servers, the notes for each version and rolling
back.

## Turn parts off

Each choice is remembered by later runs of setup and the installer.

| Command | Effect | Undo |
|---|---|---|
| `user-memory setup --no-hooks` | No index at session start and no change notices. | `--hooks` |
| `user-memory setup --no-skill` | Removes the user-memory skill. | `--skill` |
| `user-memory setup --no-instructions` | Removes your rules from Claude Code's and Codex's instruction files. | `--instructions` |
| `user-memory setup --no-update-check` | doctor stops asking GitHub whether a newer release exists. `USER_MEMORY_NO_UPDATE_CHECK=1` does the same for one run. | `--update-check` |

## Uninstall

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.sh | sh
```

It removes what setup added, each on one line, and keeps your memories:

```output
Uninstalling user memory…
  Removed: Claude Code registration
  Removed: Claude Code SessionStart hook (~/.claude/settings.json)
  Removed: Claude Code UserPromptSubmit hook (~/.claude/settings.json)
  Removed: skill (~/.agents/skills/user-memory, linked from ~/.claude/skills/user-memory)
  Removed: the user-memory rules in ~/.claude/rules/user-memory.md
  Removed: the global git ignore lines # user-memory, .user-memory, .user-memory-key (~/.config/git/ignore)
  Removed: saved setup ~/.local/bin/.user-memory.json
  Removed: the user-memory binary ~/.local/bin/user-memory
```

With Codex it removes the same for Codex, and it also removes the session
snapshots, the instruction files' write records and the saved access token
when there are any. With the installer's Docker server it
removes the server (container, image and network) and its folder, and keeps
its volume. It ends with where your memories are and how to delete them
later, for example
``Your memories were kept in the Docker volume user-memory-data. To delete them later: docker volume rm user-memory-data``.
Memories on a server elsewhere stay there: uninstall never touches a
server.

Your other settings are kept. `settings.json` and `hooks.json`
keep their other content and its key order; a file written on one line
comes back reformatted. Codex's `AGENTS.md` outside user-memory's block and
your git ignore's other lines stay byte for byte. Uninstall never touches a
Claude Code project or local entry, your `.user-memory` and
`.user-memory-key` files, or Codex's hook approvals, and it says so when it
finds one of them.

Options go after `sh -s --`:

| Option | Meaning |
|---|---|
| `--dry-run` | List what would be removed or kept, and change nothing. |
| `--purge` | Delete your memories too: the local store, its upgrade and repair backups and its [automatic backups](backup.md#automatic-backups), or the Docker server's volume. On a terminal it asks first. |
| `--yes` | With `--purge`, delete without asking: for a run without a terminal. |
| `--no-export` | With `--purge`, skip the export it takes before deleting. |
| `--client NAME` | `auto` (every client found, the default), `codex`, `claude` or `both`. |
| `--install-dir DIR` | Where the binary is (default `~/.local/bin`). |

To see first what it would do:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.sh | sh -s -- --dry-run
```

### Delete your memories too

With `--purge` on a terminal, it asks; Enter keeps them:

- ``Keep your memories`` (the store, or the Docker volume) [Y/n]
- after `n`: ``Export them to a file in this folder first?`` [Y/n]
- then ``Type delete to delete them for good:``. Anything else keeps them.

Without a terminal it deletes them only with `--yes`, after an export to the
current folder unless you add `--no-export`:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.sh | sh -s -- --purge --yes
```

```output
  Exported your memories to <folder>/user-memory-export-<time>.json (mode 0600)
```

Without a terminal and without `--yes`, it changes nothing:
``--purge deletes your memories; without a terminal to confirm, it also needs --yes. Nothing was changed.``

### When something is left

It exits with code 0 when it removed everything, or found nothing to
remove (``Nothing to remove.``). It exits with 1 when it had to leave
something, such as a file that is a symbolic link: each ``Left:`` line says
what and what to do, and it still removes everything else. It exits with 2
on a wrong option or a refused purge.

Without `--purge`, the automatic backups stay with the store. Files of
your own in the `backups` folder are never deleted, even with `--purge`. A
purge that cannot read the `backups` folder beside the store leaves the
store too, rather than delete it and keep its backups.

`user-memory uninstall` takes the same options and does the same, but keeps
the binary and the Docker server; `uninstall.sh` removes those too.

### Uninstall on Windows

```powershell
irm https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.ps1 | iex
```

It does the same as `uninstall.sh`, with the options as parameters
(`-DryRun`, `-Purge`, `-Yes`, `-NoExport`, `-Client NAME`, `-InstallDir DIR`),
given the way the installer's are. It also removes the PATH entry setup
added. Windows deletes no program a running agent session uses: when the
binary is in use, it is left, and the line says to close Claude Code and
Codex, then delete it. When the installed binary is missing or does not run,
the uninstaller downloads the latest release's, checks its SHA-256, and
removes the rest with it. A console without a window, as when an agent's
shell or a hidden scheduled task runs it, counts as no terminal: no one can
answer there, so it keeps your memories unless you give `-Purge -Yes`.
