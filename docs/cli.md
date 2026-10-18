# Command line

Your agents use user-memory through its MCP tools. You can run the same
operations yourself with the `user-memory` command. On a machine connected to
a server, every command below goes to that server, except the ones under
[run where the store lives](#run-where-the-store-lives).

Every command has `--help`.

- [Look](#look)
- [Filters](#filters)
- [Record and correct](#record-and-correct)
- [Forget](#forget)
- [Projects](#projects)
- [Backup and restore](#backup-and-restore)
- [Install and health](#install-and-health)
- [Run where the store lives](#run-where-the-store-lives)

## Look

| Command | Shows |
|---|---|
| `user-memory index` | Every memory in scope, one line each: an 8-character id and the title. |
| `user-memory index --project .` | The same, plus the current folder's project. |
| `user-memory index --topic 'publish a release'` | Memories about that topic first in their groups. |
| `user-memory read 68da18b4 1b827239` | Whole memories, by up to 20 ids (full, or the 8 characters the index shows). |
| `user-memory read --category workflow` | Every memory in one category. |
| `user-memory search 'language choice'` | Memories that match a topic, best first. |
| `user-memory review` | Groups of duplicates, project copies of global memories, one rule stated in several projects, overlaps and stale patterns. Changes nothing; `--keep ID` (repeatable) leaves the memories you name as they are, so review stops grouping them. |

Long results come in pages: 20 memories for `search`, 100 for `index` and
`read --category`. A page with more to come ends with
`next_cursor`; pass it as `--cursor` for the next page. `--page-size N` asks
for up to 200.

## Filters

`index`, `search` and `read --category` show global memories plus those of
the projects you list. They take:

| Option | Narrows to |
|---|---|
| `--project P` (repeatable; also `--projects`) | Adds project P: a key, a git remote URL or a local folder such as `.`. |
| `--scope global`, `project` or `all` | Global only; only the listed projects; or every project. |
| `--type T`, `--category C` | One type or category. |
| `--source explicit`, `observed` or `inferred` | One source. |
| `--status active`, `stale`, `superseded` or `all` | Active (the default) means confirmed and tentative. |
| `--origin O` | Memories from one conversation, such as `claude:session/<id>`. |

## Record and correct

A memory you record on the command line counts as confirmed by you, a global
one included: typing it is your yes. `--scope` is required:

```bash
user-memory record --type preference --category programming --scope global \
  --title 'Prefer Rust over Python' --content 'Prefer Rust over Python' \
  --reason 'Performance and strict safety'
```

```bash
user-memory record --type constraint --category programming --scope project --project github.com/you/app \
  --title 'Use Python in this repository' --content 'Use Python for this project'
```

| Option | Meaning |
|---|---|
| `--type` | `preference`, `constraint` or `value` for a rule; else `decision`, `goal`, `fact`, `behavior` or another label. |
| `--category` | A topic. Reuse one the index's `Categories` line lists. |
| `--title` | One line of at most 120 characters that states the memory. |
| `--content` | The memory in full, at most 4,000 characters. |
| `--reason`, `--context` | Why, and when it applies. Optional. |
| `--scope global` or `--scope project --project KEY` | Where it applies. `--project` takes a key, a git remote URL, or a folder such as `.`, which is stored as that folder's key. |

When similar memories exist, nothing is stored and the reply lists them. Run
`record` again with one of:

- `--confirms ID`: you said the same thing again;
- `--supersedes ID` (repeatable): you changed your mind;
- `--distinct-from ID` (repeatable, every listed one): they are different.

To change a memory, use `update` with its **full** id (`read` shows it). Give
only the fields that change; the rest stay, including the evidence, often
your own words, which `--evidence` replaces:

```bash
user-memory update 68da18b4-5d40-4746-930f-22a7c859e8f7 --title 'Ask before every push'
```

```bash
user-memory update 68da18b4-5d40-4746-930f-22a7c859e8f7 --content 'Prefer Python for prototypes' --reason 'Faster iteration'
```

To tell review that two memories are different, so it stops grouping them:

```bash
user-memory update 68da18b4-5d40-4746-930f-22a7c859e8f7 --distinct-from 1b827239-0000-4000-8000-000000000000
```

## Forget

```bash
user-memory forget 68da18b4-5d40-4746-930f-22a7c859e8f7
```

```bash
user-memory forget --origin claude:session/<id>
```

The first deletes one memory, by its full id or the 8 characters the index shows. The second deletes every memory
one conversation recorded, and the projects it linked, reported as
`unlinked`. A memory that another conversation also stated is kept and
listed as `kept`. Patterns built
on a forgotten memory become stale, listed as `staled`. A memory the forgotten
one had replaced is listed as `left_superseded`: it stays hidden until
`user-memory update ID` makes it active again. Forgetting cannot be undone,
except from a backup.

## Projects

| Command | Effect |
|---|---|
| `user-memory project-id [PATH]` | Prints the project key of PATH (default: the current folder). It fails for a folder without a project. |
| `user-memory project-id --set KEY [PATH]` | Names a folder's project in `.user-memory-key` ([details](projects.md#a-project-without-git-user-memory-key)). |
| `user-memory project-id --include FOLDER` | Lists a folder in `.user-memory`, so sessions started here load its project ([details](projects.md#a-folder-of-repositories-user-memory)). |
| `user-memory project-id --exclude FOLDER` | Removes a folder from `.user-memory`. |
| `user-memory project move OLD NEW` | Moves every memory and conversation link of project OLD to NEW, after a backup ([details](projects.md#when-a-project-gets-a-git-remote-project-move)). Runs where the store lives. |

## Backup and restore

Run these where the store lives. For the installer's Docker server, see
[back up and restore](server.md#back-up-and-restore).

### Automatic backups

The store backs itself up once a day. The first change to your memories
once the newest backup is a day old copies the store into a `backups` folder
beside it: `~/.local/share/user-memory/backups/memories-<UTC date>.sqlite3`
(on Windows `%LOCALAPPDATA%\user-memory\backups\`), readable only by you. The newest 7 are kept; older ones are deleted. Nothing
runs on a schedule, so a day without changes makes no backup. A backup that
fails never stops the change; [doctor](troubleshooting.md#from-doctor) reports
it.

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

### Export and import

An export is one JSON file you can keep anywhere, such as off this machine.

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
  file's.
- Importing an older backup brings back memories forgotten since.
- Backups do not include conversation links.
- Connected to a memory server, `export` and `import` work the same way:
  the server sends its memories into your file, or takes your file's.

To export or import only part of your memories, add the filters `read`
takes: `--scope global|project|all`, `--project KEY`, `--category`,
`--type`, `--source`, `--status` and `--origin`. Without a `--status`,
every status counts.

```bash
user-memory export work.json --scope project --project github.com/me/app
```

```bash
user-memory import memories.json --apply --category programming
```

A pattern needs the memories it was inferred from: a filtered import
skips one whose supporting memories are neither imported with it nor
already stored, and says how many it skipped. The
[reference](reference.md#exporting-or-importing-part-of-the-store) says
what happens to other links that leave the selection.

## Install and health

| Command | Effect |
|---|---|
| `user-memory doctor` | Checks the binary, the store or server, the automatic backups, semantic search, each client, the hooks, the skill, the instruction files, the git ignore and a pending project move, and says when a newer release is out. |
| `user-memory setup` | Connects your agents again; see [install](install.md#turn-parts-off) for its options. |
| `user-memory endpoint` | Prints the server address and the token file this machine uses. |
| `user-memory secure-file PATH` | Makes a file or folder readable only by you: mode 0600 (a folder 0700) on macOS and Linux, an access list for you alone on Windows. `doctor` names it when a file of user-memory's is open to others. |
| `user-memory --version` | Prints the client release this machine runs. |

## Run where the store lives

These commands work on the store itself. On a machine connected to a server
they refuse and say to run them on the server:

| Command | Effect |
|---|---|
| `user-memory backup list`, `user-memory backup now` | [Automatic backups](#automatic-backups). On a client, the refusal names the command to run on the server. |
| `user-memory project move OLD NEW` | Re-keys a project's memories and links. |
| `user-memory migrate` | Upgrades the store after a new release, with a backup first. `--check` only checks. |
| `user-memory repair` | Checks the store in full and, if it is damaged, backs it up and rebuilds its indexes. |
| `user-memory reindex` | Rebuilds the search index. |
| `user-memory embedding set --url URL --model NAME` | Chooses the embedding model for semantic search: checks it, calibrates it, saves it and embeds every memory again. |
| `user-memory embedding show` | Shows the endpoint, model, vector length and how duplicates are detected. |
| `user-memory embedding off` | Keyword search only; stored vectors are dropped. |
