# Back up, restore, export, import or move memories

An export holds every memory (active, stale and superseded), the conversations each came from, and the pairs marked as different. It holds no vectors or search index.

## Automatic daily backups

The store already backs itself up: the first write of a day copies it into the backups folder beside it (`~/.local/share/user-memory/backups/` for a local install, `%LOCALAPPDATA%\user-memory\backups\` on Windows, `/data/backups/` in the Docker server), and the newest 7 are kept. When the user asks for a backup, say so; an export is for a copy elsewhere, such as another machine.

To restore one, the user runs the steps on the command line, because a restore replaces the live store and every agent, this one included, must stop using it first:
1. The user runs `user-memory backup list` where the store lives (Docker server: `docker compose exec memory user-memory backup list` in the server's compose folder; on a client, `backup list` refuses and prints the exact command for its server). It lists each backup with its time and memory count, then the two commands that restore one.
2. Warn that restoring replaces the store: memories recorded since that backup are lost, and memories forgotten since come back. To keep recent memories, export first and import the export after the restore.
3. The user quits every agent (Docker: stops the server), then runs the two printed commands: one deletes the `-wal` and `-shm` files, the other copies the chosen backup over the store. On Windows they are PowerShell commands (`Get-Item … | Remove-Item -Force`, then `Copy-Item`); they do not run in `cmd.exe`. For the Docker server, the steps are in the server's "Back up and restore" section of its docs: `docker compose … run --rm --entrypoint rm memory -f /data/memories.sqlite3-wal /data/memories.sqlite3-shm`, then `… run --rm --entrypoint cp memory /data/backups/<name> /data/memories.sqlite3`, then start it.

Never run a restore yourself. `user-memory backup now` makes a backup at once; run it only when the user asks.

## Through the tools (agents)

Agents read and write only inside the exports directory beside the memory store, by file name. Never pass a path.

Back up:
1. Call `export` with a `name` such as `backup-2026-10-15.json`. It refuses an existing name; pass `overwrite: true` only if the user wants that file replaced.
2. Report the summary: file name and memory counts. Never read the file into the chat.
3. Tell the user where the file is, since the summary gives only its name:
   - Local install: in the exports folder beside the store, by default `~/.local/share/user-memory/exports/<name>` (`%LOCALAPPDATA%\user-memory\exports\<name>` on Windows).
   - Docker or other server: on the server, not on this machine. In the Docker server it is `/data/exports/<name>` inside the container; the user copies it out from the server's compose folder with `docker compose cp memory:/data/exports/<name> .`.
   - If unsure which: `user-memory endpoint` prints the server's address on a client connected to one, and fails on a local install.

Restore or import:
1. Call `import` with the `name` and no `apply`. This preview changes nothing stored; with `on_conflict: use-file` it briefly holds the store's write lock, about as long as applying would take.
2. Explain the summary:
   - `added`: new memories by status.
   - `unchanged`: identical memories; `origins_merged` counts those that gain conversations.
   - `normalized`: rows whose fields were cleaned up.
   - `conflicts`: same id, different contents, by field.
   - `invalid`: rows that break the rules; nothing is written while any exist.
   - `will_stale`: patterns that will go stale because a memory they cite changes.
   - `similar_to_existing`: new memories that resemble existing ones.
   - With filters (the same fields as `read`): `selected` and `skipped`; `file_selection` when the file is itself a filtered export.
   - `dropped_distinct_pairs` and `skipped.unsupported_inferences`: pairs and patterns left out because a memory they need is neither in the import nor in the store. A superseded memory whose replacement is missing is imported with its link, and counted.
3. Warn that restoring an older backup brings back memories the user has forgotten since it was made.
   A backup from before titles existed (export format version 1) imports only with a title for each memory the store does not already hold (those keep their titles), and the preview lists the ids without one. Ask the user to run `user-memory import FILE` on the command line, which lists each with its content; propose a title for each as "Titles for the upgrade" in [upgrade.md](upgrade.md) describes, get the user's approval, and pass the approved map as `titles` (CLI: `--titles FILE`).
4. If there are conflicts, offer the `on_conflict` choices: `abort` (default, writes nothing), `keep-store` (keep the store's version, still add the file's conversations) or `use-file` (take the file's version).
5. Only after the user approves the preview, call `import` again with `apply: true`, `user_confirmed: true` and the chosen `on_conflict`.
6. Report `added`, `unchanged` and `staled`. Suggest that the user rebuild embeddings where the store lives, and `review` if `similar_to_existing` is above zero:
   - Local install: `user-memory reindex`.
   - Docker or other server: on the server, not on this client, which refuses it. For the Docker server: `docker compose exec memory user-memory reindex` in the server's compose folder.

## On the command line (the user)

Moving memories between machines needs the CLI: agents cannot reach files outside the exports directory. The same commands work on a local install and on a client connected to a server, where the file is written or read on the client's machine.

To export or import part of the memories, add read's filters: `--scope`, `--project`, `--category`, `--type`, `--source`, `--status` (every status unless given) and `--origin`. A filtered import skips a pattern whose supporting memories are neither imported with it nor already stored, and counts it in `skipped.unsupported_inferences`.

On this machine:

```sh
umask 077
user-memory export backup.json             # mode 0600; --force replaces an existing file
user-memory import backup.json             # preview, writes nothing
user-memory import backup.json --apply     # add --on-conflict keep-store|use-file if needed
user-memory reindex
```

Or on the Docker server's machine, through Docker (stdout is not written 0600 for you, so set `umask 077` first):

```sh
umask 077
docker compose exec -T memory user-memory export - > backup.json
docker compose exec -T memory user-memory import - < backup.json           # preview
docker compose exec -T memory user-memory import - --apply < backup.json
docker compose exec -T memory user-memory reindex
```

Give the user these commands to run; do not run them yourself unless the user asks.
