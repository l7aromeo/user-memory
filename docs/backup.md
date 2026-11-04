# Back up and restore

Your memories are backed up in two ways: the store copies itself every day,
and you can export them to a file you keep anywhere.

- [Automatic backups](#automatic-backups)
- [Export and import](#export-and-import)
- [With a server](#with-a-server)
- [Move a native store to a server](#move-a-native-store-to-a-server)
- [What backups leave out](#what-backups-leave-out)

Run the backup commands where the store lives. For the installer's Docker
server, see [back up and restore](server.md#back-up-and-restore).

## Automatic backups

The store backs itself up once a day. The first change to your memories
once the newest backup is a day old copies the store into a `backups` folder
beside it: `~/.local/share/user-memory/backups/memories-<UTC date>.sqlite3`
(on Windows `%LOCALAPPDATA%\user-memory\backups\`), readable only by you. The newest 7 are kept; older ones are deleted.
Changes followed by a quiet stretch don't wait for the next day's change:
the server your agent runs copies them once the store has had no change for
an hour, and as the session ends, replacing that day's backup. A program
reading that backup then, such as `backup list`, delays the replace by up to
two seconds, since Windows can't replace a file while it's open. Nothing runs
on a schedule, so a store that hasn't changed since its newest backup gets
no new one. A backup that fails never stops the change;
[doctor](troubleshooting.md#from-doctor) reports it.

```bash
user-memory backup list
```

```output
Automatic backups in <store dir>/backups, newest first (the newest 7 are kept):
  memories-2026-10-18.sqlite3  made 2026-10-18T09:12Z  412 memories  1.3 MB
  memories-2026-10-17.sqlite3  made 2026-10-17T08:47Z  409 memories  1.3 MB
To restore one, stop every client and server using the store, then run, with the backup you choose:
  rm -f <store>-wal <store>-shm
  cp <store dir>/backups/memories-2026-10-18.sqlite3 <store>
```

```bash
user-memory backup now
```

```output
Backed up <store> to <store dir>/backups/memories-2026-10-18.sqlite3 (412 memories)
```

Here `<store>` is the store, `~/.local/share/user-memory/memories.sqlite3`
(on Windows `%LOCALAPPDATA%\user-memory\memories.sqlite3`) unless you chose
another, and `<store dir>` its folder. `backup now`
replaces today's backup, and names any older one it deleted.

On Windows it prints the two commands for PowerShell:

```output
To restore one, stop every client and server using the store, then run in PowerShell, with the backup you choose:
  Get-Item -Force -LiteralPath '<store>-wal', '<store>-shm' -ErrorAction Ignore | Remove-Item -Force
  Copy-Item -Force -LiteralPath '<store dir>\backups\memories-2026-10-18.sqlite3' -Destination '<store>'
```

To restore one, quit your agents, then run the two commands `backup list`
prints, with the backup you choose. Delete the log files first: SQLite
applies a leftover log to whatever store sits beside it, which would bring
later changes into the restored store. Restoring replaces the store, so
memories recorded since that backup are lost, and memories forgotten since
come back.

A backup is a whole copy of the store, as plain as the store itself: before
one leaves this machine, see
[copies that leave the machine](#copies-that-leave-the-machine).

## Export and import

An export is one JSON file you can keep anywhere, such as off this machine.
It is plain JSON: before it leaves this machine, see
[copies that leave the machine](#copies-that-leave-the-machine).

```bash
user-memory export memories.json
```

```bash
user-memory import memories.json
```

```bash
user-memory import memories.json --apply
```

- `export` writes every memory, readable only by you. `--force` replaces an
  existing file; `-` writes to stdout.
- `import` without `--apply` only shows what would change: memories added or
  unchanged, conflicts and invalid rows.
- `--apply` writes it. On a conflict it stops by default. `--on-conflict
  keep-store` keeps the stored version; `--on-conflict use-file` takes the
  file's. Its summary ends with a `next` step when one is worth taking:
  `review` when imported memories resemble stored ones, and, optionally,
  `reindex` to embed the new memories at once (search embeds them as it
  goes otherwise).
- Connected to a memory server, `export` and `import` work the same way:
  the server sends its memories into your file, or takes your file's.

To export or import only part of your memories, add the filters `read`
takes: `--scope global|project|all`, `--project KEY`, `--category`,
`--type`, `--source`, `--status` and `--origin`. Without a `--status`,
every status counts. As for `read`, `--project .` (or another folder) names
that checkout's project on this machine, so importing a file made
elsewhere selects the memories of this folder's project.

```bash
user-memory export work.json --scope project --project github.com/me/app
```

```bash
user-memory import memories.json --apply --category programming
```

A pattern needs the memories it was inferred from: a filtered import
skips one whose supporting memories are neither imported with it nor
already stored, and says how many it skipped. The
[memory model](memory-model.md#exporting-or-importing-part-of-the-store) says
what happens to other links that leave the selection.

## With a server

The server backs its store up every day, as a native store does.
`user-memory export FILE` and `user-memory import FILE` work from any machine
connected to it: the file is written or read on that machine. An agent's
export lands in the server's exports folder instead, so for a copy on your
own machine, run `user-memory export FILE` there. The Docker server's own
backups and restore steps are in
[back up and restore](server.md#back-up-and-restore).

## Move a native store to a server

Export on the old machine, import on the server, then run the installer on
the old machine with the server's address. Move the export as
[copies that leave the machine](#copies-that-leave-the-machine) says, and
delete it once it is imported.

## Copies that leave the machine

user-memory doesn't encrypt exports or backups: each is plain data, readable
by anyone who gets the file. The store's own machine is covered by disk
encryption ([privacy](privacy.md#what-is-stored)); a copy elsewhere is not.

- Encrypt a copy before it leaves the machine, with a tool you trust, such
  as `age` or an encrypted archive, or keep it on a disk you encrypt.
- Move it over an encrypted channel, such as `scp`, never by email or chat.
- Delete the copy once you no longer need it, such as after an import.

Which copies still hold a forgotten memory, and for how long, is in
[privacy](privacy.md#what-a-forgotten-memory-leaves).

## What backups leave out

An automatic backup is a whole copy of the store. An export holds the
memories, their conversations and the pairs marked as different, and leaves
out:

- conversation links, which belong to conversations, not to memories
  ([links](projects.md#another-repository-for-one-conversation-links));
- embedding vectors and the search index, which are rebuilt from the
  memories.

Restoring an automatic backup replaces the store: memories recorded since
are lost, and memories forgotten since come back. Importing an older export
brings back memories forgotten since and leaves the rest as they are.
