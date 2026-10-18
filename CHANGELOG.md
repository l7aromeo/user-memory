# Changelog

What changed in each release, newest first.

## v0.7.0

### Upgrade

Before you upgrade from 0.6.x, keep an export of your memories:

```bash
user-memory export memories-before-0.7.0.json
```

Releases before 0.7.0 can no longer be downloaded, so after the upgrade
0.6.x cannot be reinstalled; the export keeps your memories in a form any
later version imports. In 0.6.x, export runs where the store lives: for the
installer's Docker server, on its machine, through Docker, as
[back up and restore](https://github.com/l7aromeo/user-memory/blob/main/docs/server.md#back-up-and-restore)
shows. Then run the installer again:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

- The store keeps its format, so nothing is migrated and nothing needs your
  input.
- Restart Claude Code and Codex, and start new sessions.
- Codex's hooks are unchanged on macOS and Linux, so Codex needs no new
  approval.
- A shared server behind a reverse proxy: route `/export` and `/import` to
  the server too, beside `/mcp`, with request bodies up to 129 MiB
  (`client_max_body_size 129m` in nginx). Clients use them for export and
  import (see below). The
  [nginx example](https://github.com/l7aromeo/user-memory/blob/main/docs/server.md#a-shared-server-behind-https)
  has the block to add. Without it, everything else works as before.

### New

- **Windows (preview).** user-memory runs on Windows 10 and 11, x64 and
  ARM64. It is tested in CI on both, but not yet with Claude Code and Codex
  on a real Windows machine; please report what you find in an
  [issue](https://github.com/l7aromeo/user-memory/issues). In PowerShell:

  ```powershell
  irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1 | iex
  ```

  It asks the same questions as on macOS and Linux and takes the same
  options as PowerShell parameters (`-Local docker`, `-NoEmbed`, `-Help`).
  The client goes to `%LOCALAPPDATA%\Programs\user-memory\bin`, which setup
  adds to your user PATH; the store and token live in
  `%LOCALAPPDATA%\user-memory`, readable only by you. The Docker server runs
  in Docker Desktop, and the installer offers Ollama on this PC for semantic
  search. `uninstall.ps1` removes it as `uninstall.sh` does, with the same
  options as parameters. See
  [install on Windows](https://github.com/l7aromeo/user-memory/blob/main/docs/install.md#install-on-windows).
  - The Windows binaries are not code-signed yet. The installer checks each
    download's SHA-256 against the release before it runs it, and says so
    when Smart App Control or an antivirus blocks it; you can check a
    download yourself with `Get-FileHash`.
  - Claude Code's hooks on Windows need Claude Code 2.1.139 or newer: run
    `claude update` first. Setup still writes the hooks for an older one,
    and setup and `user-memory doctor` then ask you to run `claude update`.
  - Codex inside the ChatGPT app is not found on Windows; install the Codex
    CLI.
- **Export and import from any machine.** On a client connected to a memory
  server, `user-memory export FILE` and `user-memory import FILE` now work as
  with a local install: the server sends its memories into a file on your
  machine, or takes your file's. Before, they refused and had to be run where
  the store lives.
- **Export or import part of your memories.** Both take the filters `read`
  takes: `--scope`, `--project`, `--category`, `--type`, `--source`,
  `--status` and `--origin`; without `--status`, every status counts. A
  filtered import skips a pattern whose supporting memories are neither in
  it nor already stored, and says how many it skipped. The summary gains
  `selected`, `skipped`, `links_outside` and, for a file that is itself a
  filtered export, `file_selection`.
- **Store privacy check.** `user-memory doctor` checks that only you can
  read the store, and when another account can, prints the command that
  fixes it. `user-memory secure-file PATH` makes a file or folder yours
  alone: mode 0600 (0700 for a folder) on macOS and Linux, an access list on
  Windows.
- **Backup commands on a client.** `user-memory backup list` and
  `backup now` on a client of a server print the exact command to run
  instead: with the installer's compose file when this machine runs the
  Docker server, else for the machine the client connects to.

### Fixed

- An export made after you forgot a memory's replacement now imports into a
  fresh store: the superseded memory keeps its link to the missing
  replacement, and the summary counts it.
- The restore commands `user-memory backup list` prints always quote their
  paths, as the hook commands do.
- When the daily backup failed during setup, for a store reached through a
  symbolic link, setup tried it again at once and reported the failure
  twice. It is now tried and reported once.
- Setup, doctor and uninstall no longer take a client that crashed while
  answering for one without user-memory: they report the failed query, and
  uninstall leaves that registration with what to run. A client command that
  fails without a word is reported with its exit status or signal.

## v0.6.1

### Upgrade

Run the installer again:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

- The store keeps its format, so nothing is migrated and nothing needs your
  input.
- Restart Claude Code and Codex, and start new sessions.
- Codex's hooks are unchanged, so Codex needs no new approval.

### Fixed

- Changing a memory no longer erases its evidence, which is often your own
  words. Before, `user-memory update`, and the update tool agents use,
  cleared the evidence whenever they changed anything besides the title
  alone, such as its content. The evidence now stays unless the change gives
  new evidence: `--evidence` (the tool's `evidence`) replaces it, and an empty
  value clears it. Confirming an observation keeps what was observed.

## v0.6.0

### Upgrade

Run the installer again:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

- Your store is upgraded in one step, with a backup beside it first. It gains
  a table for the duplicate settings of embedding models. Your memories come
  through unchanged, and nothing needs your input.
- Setup then makes the store's first automatic backup (see below).
- Restart Claude Code and Codex, and start new sessions.
- Semantic search with a model other than `qwen3-embedding:4b`:
  `user-memory doctor` asks you once to run the `user-memory embedding set …`
  command it prints, which sets up duplicate detection for that model.
- The installer keeps your semantic search choice. The Docker server's
  setting moves into `embedding.env` beside its `compose.yaml`.
- Codex's hooks are unchanged, so Codex needs no new approval.
- To go back to 0.5.0, use the backup the upgrade made (`.v4-<time>.bak`) as
  [roll back an upgrade](https://github.com/l7aromeo/user-memory/blob/main/docs/install.md#roll-back-an-upgrade)
  describes. The daily backups are copies of the upgraded store, which 0.5.0
  refuses.

### New

- **Daily backups.** The first change to your memories each day copies the
  store into a `backups` folder beside it, readable only by you, and the
  newest 7 are kept. A memory you forget stays in the backups until 7 newer
  daily backups replace them: a week if your memories change every day,
  longer if they change less often. Other writes never wait for a backup,
  and a failed backup never fails the change it follows.
  `user-memory backup list` shows them with the commands to restore one, and
  `user-memory backup now` makes one at once. `user-memory doctor` reports
  the newest and asks you to act when there is none, when it is over two days
  old while your memories changed, or when the last one failed. Uninstall
  keeps them with your memories, and `--purge` deletes them too.
- **Choose how to search by meaning.** For a server on this machine, the
  installer asks: Ollama on this machine (it lists the embedding models Ollama
  has, recommends `qwen3-embedding:4b`, and offers to install Ollama or
  download the model, asking first), another OpenAI-compatible endpoint with
  an API key saved readable only by you, or keywords only. Without a
  terminal, `--embed-model`, `--embed-url`, `--embed-key-file`,
  `--embed-dimensions`, `--no-embed`, `--install-ollama` and `--pull-model`
  give the same answers.
- **`user-memory embedding`.** `embedding set --url <address> --model <name>`
  checks the endpoint, sets up the model and embeds your memories again;
  `embedding show` says what is in use, and `embedding off` returns to
  keyword search. For the installer's Docker server it updates and restarts
  the server.
- **Any embedding model catches duplicates.** A model other than
  `qwen3-embedding:4b` is calibrated on example memory pairs that ship with
  user-memory, so record-time duplicate checks and review use it too. A model
  that cannot tell the examples apart still searches by meaning, and
  `embedding set` and doctor say so.
- **A server-only host.** `install.sh --server-only` sets up the Docker server
  on a host with no Claude Code or Codex, connects no agent there, and ends
  with how other machines reach it. Doctor on that host checks the server,
  its store and its backups.
- **New release notice.** `user-memory doctor` says when a newer release is
  out: ``user-memory 0.7.0 is available (this is 0.6.0): run the installer
  again``. It asks GitHub's releases page only, never during a session, and
  `user-memory setup --no-update-check` turns it off.
- **Any folder in `.user-memory`.** `@` lines and `user-memory project-id
  --include` take any path: relative ones with `..`, absolute ones, `~/` and
  links to folders.

### Fixed

- Review no longer suggests deleting a rule that two of your projects state
  when no global memory covers it. It lists such a rule as shared by those
  projects, which you may choose to make one global rule.
- Review's groups of related memories hold only memories that relate to each
  other, instead of chains of loosely linked ones. A group of duplicates or
  stale patterns too large to list says how many it left out.
- Text that starts with `-`, such as a Markdown list item, works as a value
  for `user-memory record` and `update`, as in `--content "- item"`.
- Setup and doctor work inside a git checkout that git cannot read, such as a
  worktree copied from another machine; doctor names that checkout in a note.
- A memory that was stored is never reported as failed when SQLite's
  follow-up copy of its log fails.
- When the Docker server on Linux cannot reach Ollama on this machine, the
  installer names the fix: let Ollama listen on the Docker bridge address.

## v0.5.0

### Upgrade

Run the installer again:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

- Your store is upgraded in one step, with a backup beside it first. Your
  memories come through unchanged, and nothing needs your input.
- Restart Claude Code and Codex, and start new sessions.
- On the command line, `user-memory record` now needs `--scope global` or
  `--scope project`.
- Still on 0.3? Upgrade through 0.4's titles step first:
  [upgrade from 0.3](https://github.com/l7aromeo/user-memory/blob/main/docs/install.md#upgrade-from-03-titles).
- Codex's hooks are unchanged, so Codex needs no new approval.
- To go back to 0.4.0, see
  [roll back an upgrade](https://github.com/l7aromeo/user-memory/blob/main/docs/install.md#roll-back-an-upgrade).

### New

- **Projects without git.** A new project without a git remote can be named
  with `user-memory project-id --set <key>`, which writes the key to
  `.user-memory-key` in its folder.
- **Folders of repositories.** List the repositories under a parent folder in
  its `.user-memory`, one `@folder` line each, with `user-memory project-id
  --include <folder>` and `--exclude <folder>`. Sessions started there load
  their projects too.
- **Links.** An agent can link another repository to the conversation. Its
  memories stay loaded for that conversation through resume and compaction.
- **Moving a project.** When a project gets a git remote after it started
  under a key file, or when a repository is renamed or moves to another
  owner or host, `user-memory project move <old> <new>` moves its memories
  and links to the new key, after a backup.
- **What a session loads.** The session start names every project it loads,
  grouped by where each comes from. In a folder without a project, it names
  the repositories inside with their keys. Before a prompt, the agent is told
  when the loaded projects change.
- **Global memories need your yes.** An agent must ask before it records a
  memory that applies in every project.
- **A shared server from the installer.** `--local docker --allowed-host
  <host>`, and `--bind <address>` for a proxy on another machine, set up a
  server that all your machines reach through an HTTPS proxy.
- **Git ignore.** Setup adds `.user-memory` and `.user-memory-key` to your
  global git ignore, so they are never committed.
- **Steps for you, not errors.** Hooks Codex has not approved yet, a skill
  that setup did not install, a Claude Code setting that overrides the
  shared server in one folder, and a git ignore that lacks the two lines are
  now steps to take. `user-memory doctor` names each on an `Action needed:`
  line and exits with code 3; setup and the installer finish, and the
  installer lists the steps last.
- **Uninstall.** One command removes everything setup added and keeps your
  memories; `--dry-run` shows what it would do first, and `--purge` deletes
  the memories too, after an export:

  ```bash
  curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.sh | sh
  ```

  `user-memory uninstall` does the same but keeps the binary and the Docker
  server.
- **User docs.** Guides for installing, projects, the command line, the
  server, troubleshooting and privacy are now in this repository's
  [docs](https://github.com/l7aromeo/user-memory/tree/main/docs).

### Fixed

- Correcting a memory keeps the conversations it came from, and adds the
  one that corrects it. When you restate a memory in full in a new
  conversation, the agent can make that conversation its only one instead.
- A session index too long for the session start lists what fits, rules
  first, and counts the rest of each group; global memories and the folder's
  own project stay whole when other projects load too.
- `user-memory index` says ``No memories match these projects and filters.``
  when the store holds memories but none match, instead of calling the store
  empty.
- `user-memory record` and `update` store a folder given as `--project`, such
  as `.`, as that folder's project key, never as the path.
- When `user-memory doctor` finds a problem, the installer still prints how
  to use and manage the server before it fails.

