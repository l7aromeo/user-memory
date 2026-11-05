# Upgrade

How to upgrade a machine and a shared server, what changed in each version
that needs a step from you, and how to go back.

- [Upgrade a machine](#upgrade-a-machine)
- [Upgrade a shared server](#upgrade-a-shared-server)
- [Notes for each version](#notes-for-each-version)
- [Roll back an upgrade](#roll-back-an-upgrade)
- [A store too old to upgrade](#a-store-too-old-to-upgrade)

## Upgrade a machine

Run the installer again:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

On a machine that is already set up, it asks nothing:

- With an existing server, it keeps that server and token. Upgrade the server
  too ([upgrade a shared server](#upgrade-a-shared-server)). A client refuses a server that is
  too old for it and says to update the server.
- With the installer's Docker server, it builds the new image, checks the
  store, stops the server, upgrades the store and starts the server again. It
  keeps the port, the embedding choice and the token.
- With a native store, setup upgrades the store in place.

Before any change to the store's format, the upgrade copies the store to a
backup file beside it, then upgrades it in one step. A native store prints:

```output
Backed up <store> to <store>.v4-<time>.bak
Migrated <store> from schema version 4 to 5.
```

The installer's Docker server does the same inside its volume, with the
backup at `/data/memories.sqlite3.v4-<time>.bak`. Your memories come
through unchanged. If the upgrade fails, the store is left as it was. Keep
the backup's name: [rolling back](#roll-back-an-upgrade) needs it.

Setup also makes the store's first [automatic backup](backup.md#automatic-backups)
when it has none from the last day, so an upgraded store is backed up daily
from the start.

After an upgrade, restart your agents and start new sessions. Old sessions
keep the tools they started with.

## Upgrade a shared server

For the installer's Docker or native server, run the installer again. It
backs the store up before it changes the store's format
([upgrade a machine](#upgrade-a-machine)). For the Docker server, the backup stays in
the volume, at `/data/memories.sqlite3.v<old version>-<time>.bak`.

The server never upgrades its store itself: until the installer has upgraded
it, the server refuses it with
``run the installer again (or ./setup, or `user-memory migrate`) to upgrade it``.
An upgrade from 0.3 needs a title for each memory
([titles](#upgrade-from-03-titles)).

While the server restarts, a client's call gets
``The memory server at … did not answer; the call may not have completed``
and the next call reconnects ([troubleshooting](troubleshooting.md#from-doctor)).
Clients don't restart for a server restart or upgrade, only when their own
client is upgraded.

To undo an upgrade, restore the backup and run the previous version's
installer with its tag: [roll back an upgrade](#roll-back-an-upgrade)
gives the steps for the Docker server and a native store. Memories recorded
since the upgrade are lost unless you exported them first.

## Notes for each version

Each release's notes say what changed; [the changelog](../CHANGELOG.md)
collects them. The versions below needed something more than running the
installer again.

### Upgrade to 0.9: the keyword index

0.9 replaces the store's unused copy of every memory's text (schema version
5) with a [keyword index](storage.md#the-keyword-index) of each memory's
stems (schema version 6), so search stays fast as the store grows. Run the
installer again on each machine and on the server: it backs the store up
beside itself (`<store>.v5-<UTC time>.bak`), then migrates it and builds the
index in one transaction. No memory, conversation or vector changes. A
store of 20,000 memories took under a second when measured, and one of
100,000 from 5 to 16 seconds, depending on how long the memories are.

A 0.8 binary refuses a version 6 store, as with every schema change, so
upgrade every machine that opens the store; to go back, follow [roll back an
upgrade](#roll-back-an-upgrade).

### From 0.6 to 0.8

The store's format did not change from 0.6 to 0.8, so upgrading within them
changes no file and makes no store backup. Run the installer again on each
machine and on the server.

### Upgrade from 0.5 to 0.6

The store gains a table for the duplicate cutoffs of embedding models. The
upgrade backs it up first, as above, and needs nothing from you. What
changes for you:

- The store backs itself up every day, keeping a week of copies beside it
  ([automatic backups](backup.md#automatic-backups)). Setup makes the first one.
- [Semantic search](install.md#semantic-search) takes any embedding model. With a
  model other than `qwen3-embedding:4b`, doctor asks you once to run the
  `user-memory embedding set …` command it prints, which calibrates duplicate
  detection for that model. The upgrade keeps your choice; the Docker
  server's setting moves into `embedding.env` beside its `compose.yaml`.
- `--server-only` sets up the Docker server on a host with no agent app
  ([choices](install.md#choices-the-installer-offers)).
- `.user-memory` lines can name any folder: `..`, absolute and `~/` paths
  ([projects](projects.md#a-folder-of-repositories-user-memory)).
- Text that starts with `-`, such as a list item, works as a value on the
  command line: `user-memory update <id> --content "- item"`.

### Upgrade from 0.4 to 0.5

The store gains a table for conversation links. The upgrade backs it up
first, as above, and needs nothing from you. What changes for you:

- Agents ask before they record a memory as global
  ([why](how-it-works.md#scope-global-or-one-project)). On the command line,
  `record` now needs `--scope global` or `--scope project`.
- A folder without git, or a folder of repositories, can have projects, and
  an agent can link another repository to a conversation
  ([projects](projects.md)).
- Setup adds `.user-memory` and `.user-memory-key` to your global git
  ignore, so those files are never committed, and says so:

  ```output
  Git ignore: .user-memory and .user-memory-key added to <git ignore file>
  ```

  The file is the one `git config --global core.excludesFile` names, else
  `~/.config/git/ignore`. It gains three lines:

  ```
  # user-memory
  .user-memory
  .user-memory-key
  ```

### Upgrade from 0.3: titles

Since 0.4 every memory has a title: one line that states the memory, which
the index shows. Nothing writes titles for you, so the upgrade from 0.3 stops,
changes nothing, and lists your memories that need one. With the installer's
Docker server, the old server keeps running.

1. Ask your agent: "upgrade my memory store". The user-memory skill proposes
   a title for each memory.
2. Read the list and change what you want. Approve it when every title is
   right.
3. The agent writes the approved titles to a file such as `titles.json`.
4. Run the installer again with that file:

   ```bash
   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --titles titles.json
   ```

If it still refuses, it names the memories that are missing a title, or that
have one that is empty or longer than 120 characters. Fix those and run it
again. For a server you run from a checkout, see
[upgrade a shared server](#upgrade-a-shared-server).

## Roll back an upgrade

To go back to the previous version, put the backup back and install that
version with its installer and its tag. Memories recorded or changed since
the upgrade are lost; to keep them, [export](backup.md#export-and-import)
first and keep the file.

This works for any release from 0.7.0 on. Releases before 0.7.0 can no
longer be downloaded, so they cannot be reinstalled: before upgrading from
one of them, keep an export (`user-memory export FILE`), which any later
version imports.

The steps below go back to 0.7.0; for another release, use its tag. Use
the backup the upgrade made, `<store>.v<N>-<time>.bak`, where N is the
store's version before the upgrade: the automatic backups in the `backups`
folder are copies of the upgraded store, which the previous version
refuses, as it refuses the upgraded store until the backup is back:
``this user-memory is older than the store``.

**Native store:**

1. Quit your agents, so nothing holds the store open.
2. Delete the store's write-ahead log files, if they are there. SQLite
   applies a leftover log to whatever store sits beside it, which would bring
   writes made after the backup into the restored store:

   ```bash
   rm -f ~/.local/share/user-memory/memories.sqlite3-wal ~/.local/share/user-memory/memories.sqlite3-shm
   ```

3. Copy the backup over the store, with the name the upgrade printed:

   ```bash
   cp ~/.local/share/user-memory/memories.sqlite3.v<N>-<time>.bak ~/.local/share/user-memory/memories.sqlite3
   ```

   On Windows, steps 2 and 3 in PowerShell:

   ```powershell
   Get-Item -Force -LiteralPath "$env:LOCALAPPDATA\user-memory\memories.sqlite3-wal", "$env:LOCALAPPDATA\user-memory\memories.sqlite3-shm" -ErrorAction Ignore | Remove-Item -Force
   Copy-Item -Force -LiteralPath "$env:LOCALAPPDATA\user-memory\memories.sqlite3.v<N>-<time>.bak" -Destination "$env:LOCALAPPDATA\user-memory\memories.sqlite3"
   ```

4. Run the previous version's installer with its tag:

   ```bash
   curl -fsSL https://github.com/l7aromeo/user-memory/releases/download/v0.7.0/install.sh | sh -s -- --version v0.7.0
   ```

   On Windows, with `install.ps1` and `-Version`:

   ```powershell
   & ([scriptblock]::Create((irm https://github.com/l7aromeo/user-memory/releases/download/v0.7.0/install.ps1))) -Version v0.7.0
   ```

5. Restart your agents.

**The installer's Docker server:** stop it, delete the log files and copy the
backup over the store inside the volume, then run the previous version's
installer. The upgrade
rebuilt the server image under the same name, so the old image is gone; the
previous installer builds it again and starts it on the restored store. On
Windows, in PowerShell, give `docker compose` the file
`-f "$env:LOCALAPPDATA\user-memory\docker\compose.yaml"` instead.

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml stop memory
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint rm memory -f /data/memories.sqlite3-wal /data/memories.sqlite3-shm
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint cp memory /data/memories.sqlite3.v<N>-<time>.bak /data/memories.sqlite3
```

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/download/v0.7.0/install.sh | sh -s -- --version v0.7.0
```

Then run the same installer on every machine that uses this server, so the
clients match it. Your agent can also walk you through these steps: ask it
to "roll back my memory store upgrade".

## A store too old to upgrade

A store from before schema version 2, or a file that is
not a user-memory store, is refused with
``… needs schema version … Move it aside``. Move the file aside and run
setup to start a fresh store, then import an export made with the version
that created the old one. Your agent can walk you through it: ask it to
"upgrade my memory store".
