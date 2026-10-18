# Upgrade a memory store

Use this when `user-memory setup`, `user-memory doctor`, the client or the session hook refuses the store because of its schema version. A refused store is always left unchanged. Pick the case by the message.

## "run the installer again (or ./setup, or `user-memory migrate`) to upgrade it"

The store is from an older version that this one can upgrade.

1. Tell the user to run the installer again (`curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh`), or `./setup` from the new version's checkout, or `user-memory setup`. It backs the store up first, then upgrades it in one transaction. If it stops with "needs a title for every memory", follow "Titles for the upgrade" below first.
2. Ask the user for the line `Backed up … to …` that setup prints, and tell them where the backup is: `<store>.v<old version>-<UTC time>.bak` beside the store, mode 0600. Setup never replaces an existing backup.
3. The user restarts their clients, then runs `user-memory doctor`.

If the migration fails, it rolls back: the store is unchanged and the message names the backup. If it refuses because the store "is damaged", nothing was changed or backed up: the user runs `user-memory repair` (Docker: `docker compose run --rm memory repair`), which backs it up and rebuilds its indexes, then upgrades again.

Docker server: `migrate` upgrades `/data/memories.sqlite3`, and the backup lands beside it in the user-memory-data volume.

```sh
docker compose run --rm memory migrate --check && docker compose stop memory && docker compose run --rm memory migrate && docker compose up -d memory
```

### Titles for the upgrade

Schema version 3 gives every memory a title: one line of at most 120 characters that states the memory so an agent can act on it. Nothing writes titles for the user, so the upgrade refuses, changing nothing, until it has one for each memory, and lists the untitled ones with their content.

1. Get the list: the refusal from the installer, `./setup` or `user-memory migrate` (Docker: `docker compose run --rm memory migrate --check`, which changes nothing).
2. Propose a title for each memory: the rule or fact itself, such as "Push, publish or tag only with the user's explicit go-ahead", not a topic such as "Release policy". Keep the user's wording where it is already short.
3. Show every proposed title to the user next to its memory, and change what they ask for. Write nothing until they approve the whole list.
4. Write the approved map to a file, one JSON object from each memory id to its title, for example `titles.json`:

   ```json
   {"68da18b4-…": "Push, publish or tag only with the user's explicit go-ahead"}
   ```

5. Run the upgrade with it: the installer again with `--titles titles.json` (`… | sh -s -- --titles titles.json`), or `./setup --titles titles.json`, or `user-memory migrate --titles titles.json`. Docker server from a checkout: `docker compose run --rm -T memory migrate --titles - < titles.json`. Each title is stored as written, with its whitespace tidied.
6. If it refuses again, it names the ids still missing, unknown or invalid; fix those entries and repeat.

## "this user-memory is older than the store"

The store was upgraded by a newer version. Nothing needs migrating: the user updates user-memory (run the installer again, or pull the newer checkout and run `./setup`, or update the Docker image) and restarts the client. Never move the store aside for this.

## "needs schema version … Move it aside"

The store is too old, unversioned or not a user-memory store, and cannot be upgraded in place. The user runs these steps; explain them and wait for each result:

1. Export with the version that created the store, before installing the new one (or with that version's binary kept aside):

   ```sh
   umask 077
   user-memory export memories-before-upgrade.json
   ```

   This works from the first version that has `export`. A store from an older version cannot be exported; it can only be moved aside and started fresh.
2. Move the store aside, for example rename `~/.local/share/user-memory/memories.sqlite3` (on Windows `%LOCALAPPDATA%\user-memory\memories.sqlite3`) with a `.bak` suffix. Keep it until the import is verified.
3. Install the new version and run `./setup` (or `user-memory setup`) to create a fresh store.
4. Preview, then apply the import:

   ```sh
   user-memory import memories-before-upgrade.json
   user-memory import memories-before-upgrade.json --apply
   ```

5. Run `user-memory reindex`, then `user-memory doctor`.
6. Once the user has checked their memories, they may delete the `.bak` store and the export file.

For a Docker server, run export and import through `docker compose exec -T memory user-memory …` as in [backup.md](backup.md).

## Roll back an upgrade

Only on the user's request. Restoring the backup loses every memory recorded or changed since the upgrade; if they want to keep those, export first (`user-memory export FILE`, see [backup.md](backup.md)). A rollback reinstalls a release from 0.7.0 on: releases before 0.7.0 can no longer be downloaded, so they cannot be reinstalled. If the user wants to go back before 0.7.0, say so; an export they kept still imports into any later version.

1. Quit the clients so nothing holds the store open.
2. Delete the store's write-ahead log files if they are there: `rm -f ~/.local/share/user-memory/memories.sqlite3-wal ~/.local/share/user-memory/memories.sqlite3-shm` (on Windows, in PowerShell: `Get-Item -Force -LiteralPath "$env:LOCALAPPDATA\user-memory\memories.sqlite3-wal", "$env:LOCALAPPDATA\user-memory\memories.sqlite3-shm" -ErrorAction Ignore | Remove-Item -Force`). SQLite applies a leftover `-wal` to whatever store sits beside it, which would bring writes made after the backup into the restored store.
3. Copy the backup over the store, for example `cp ~/.local/share/user-memory/memories.sqlite3.v5-20261018T084808Z.bak ~/.local/share/user-memory/memories.sqlite3` with the backup name setup printed (on Windows: `Copy-Item -Force -LiteralPath "$env:LOCALAPPDATA\user-memory\<backup name>" -Destination "$env:LOCALAPPDATA\user-memory\memories.sqlite3"`). Use that upgrade backup, never one from the backups folder beside the store: those daily copies were made after the upgrade, in the new version's format, which the previous version refuses.
4. Install the previous version, then restart the clients:
   - Installed with the installer: run that version's installer with its tag, such as `curl -fsSL https://github.com/l7aromeo/user-memory/releases/download/v0.7.0/install.sh | sh -s -- --version v0.7.0`.
   - From a checkout: run `./setup` from the previous version's checkout.

Docker server set up by the installer: stop it, delete the log files and copy the backup inside the volume, then run the previous version's installer as in step 4. The upgrade replaced the server image, so that installer builds the previous one again and starts it on the restored store:

```sh
docker compose -f ~/.local/share/user-memory/docker/compose.yaml stop memory
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint rm memory -f /data/memories.sqlite3-wal /data/memories.sqlite3-shm
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint cp memory /data/memories.sqlite3.v5-20261018T084914Z.bak /data/memories.sqlite3
curl -fsSL https://github.com/l7aromeo/user-memory/releases/download/v0.7.0/install.sh | sh -s -- --version v0.7.0
```

A server run from the repository's `compose.yaml`: stop it, delete the log files and copy the backup the same way, then check out the previous version and run `docker compose up -d --build`.
