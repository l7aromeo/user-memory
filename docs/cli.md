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
- [Run by the clients](#run-by-the-clients)

## Look

| Command | Shows |
|---|---|
| `user-memory index` | Every memory in scope, one line each: an 8-character id and the title. |
| `user-memory index --project .` | The same, plus the current folder's project. |
| `user-memory index --topic 'publish a release'` | Memories about that topic first in their groups. |
| `user-memory index --instructions` | The rules text the instruction files hold. |
| `user-memory read 68da18b4 1b827239` | Whole memories, by up to 20 ids (full, or the 8 characters the index shows). `--status all` includes stale and superseded ones. |
| `user-memory read --category workflow` | Every memory in one category. |
| `user-memory search 'language choice'` | Memories that match a topic, best first. |
| `user-memory review` | Groups of duplicates, project copies of global memories, one rule stated in several projects, overlaps and stale patterns. Changes nothing; `--keep ID` (repeatable) leaves the memories you name as they are, so review stops grouping them. `--project P` adds projects; `--max-groups N` limits the groups listed. |

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
one included: typing it is your yes. So do `update`, `forget` and
`review --keep`. A program that runs these commands for you, such as an
agent with a shell, gives that yes too ([what the server can
check](memory-model.md#what-the-server-can-and-cannot-check)). `--scope` is
required:

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

`--supersedes` with `--merge` merges two or more duplicates you approved,
keeping all their conversations. `--origin ORIGIN` names the conversation
a memory came from, as agents do with the session's origin. Observations
and patterns recorded on the command line stay tentative.

To change a memory, use `update` with its **full** id (`read` shows it). Give
only the fields that change; the rest stay, including the evidence, often
your own words, which `--evidence` replaces:

```bash
user-memory update 68da18b4-5d40-4746-930f-22a7c859e8f7 --title 'Ask before every push'
```

```bash
user-memory update 68da18b4-5d40-4746-930f-22a7c859e8f7 --content 'Prefer Python for prototypes' --reason 'Faster iteration'
```

`--evidence` replaces the kept evidence, `--origin ORIGIN` adds the
conversation the correction came from, and `--replace-origins` with it keeps
only that one:

```bash
user-memory update 68da18b4-5d40-4746-930f-22a7c859e8f7 --content 'Prefer Python for prototypes' --origin claude:session/<id> --replace-origins
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
except from a backup. Nothing is ever deleted automatically, so write a
memory's content, context and reason to read well without the chat it came
from.

Forgetting removes the memory's text from the store and from every backup
made after. Copies made before keep it, and restoring one brings the memory
back: the automatic backups, the `.bak` files beside the store and your
exports. [Privacy](privacy.md#what-a-forgotten-memory-leaves) lists every
copy and how long each lasts. To remove every copy, such as after a secret
was stored by mistake:

1. Run `user-memory backup now` where the store lives, for a backup without
   it. On a store upgraded from an earlier version, do this after setup or
   `user-memory migrate` has rewritten the store
   ([removed text](storage.md#removed-text)).
2. Delete the older backups that `user-memory backup list` shows, the
   `.bak` files beside the store, and every export made before the forget.
   You can no longer go back to any of them.

## Projects

| Command | Effect |
|---|---|
| `user-memory project-id [PATH]` | Prints the project key of PATH (default: the current folder). It fails for a folder without a project. |
| `user-memory project-id --set KEY [PATH]` | Names a folder's project in `.user-memory-key` ([details](projects.md#a-project-without-git-user-memory-key)). |
| `user-memory project-id --include FOLDER` | Lists a folder in `.user-memory`, so sessions started here load its project ([details](projects.md#a-folder-of-repositories-user-memory)). |
| `user-memory project-id --exclude FOLDER` | Removes a folder from `.user-memory`. |
| `user-memory project move OLD NEW` | Moves every memory and conversation link of project OLD to NEW, after a backup ([details](projects.md#when-a-project-gets-a-git-remote-project-move)). Runs where the store lives. |

## Backup and restore

[Back up and restore](backup.md) explains each of these.

| Command | Effect |
|---|---|
| `user-memory export FILE` | Writes every memory, or those the filters select, to FILE (`-` for stdout), readable only by you. `--force` replaces an existing file. |
| `user-memory import FILE` | Shows what importing FILE (`-` for stdin) would change; `--apply` writes it, `--on-conflict abort\|keep-store\|use-file` settles conflicts, and `--titles FILE` gives titles to a file from before 0.4. |
| `user-memory backup list` | The automatic backups, newest first, and how to restore one. Runs where the store lives. |
| `user-memory backup now` | Backs the store up now, replacing today's backup. Runs where the store lives. |

Export and import take the filters `read` takes; on a client they go to the
server, with the file on your machine.

## Install and health

| Command | Effect |
|---|---|
| `user-memory doctor` | Checks the binary, the store or server, the automatic backups, semantic search, each client, the hooks, the skill, the instruction files, the git ignore and a pending project move, and says when a newer release is out. It exits with 0 when all is well, 3 when only `Action needed:` steps are left, and 1 on an error. `--server-only` checks only the store, for a host with no agent app. |
| `user-memory setup` | Connects your agents again; [configuration](configuration.md#setup-options) lists its options, and [turn parts off](install.md#turn-parts-off) the common ones. |
| `user-memory endpoint` | Prints the server address and the token file this machine uses. |
| `user-memory secure-file PATH` | Makes a file or folder readable only by you: mode 0600 (a folder 0700) on macOS and Linux, an access list for you alone on Windows. `doctor` names it when a file of user-memory's is open to others. |
| `user-memory --version` | Prints the client release this machine runs. |

## Run where the store lives

These commands work on the store itself. On a machine connected to a server
they refuse and say to run them on the server:

| Command | Effect |
|---|---|
| `user-memory backup list`, `user-memory backup now` | [Automatic backups](backup.md#automatic-backups). On a client, the refusal names the command to run on the server. |
| `user-memory project move OLD NEW` | Re-keys a project's memories and links. |
| `user-memory migrate` | Upgrades the store after a new release, with a backup first. `--check` only checks, `--titles FILE` gives titles to a store from 0.3, and `--db PATH` names another store. |
| `user-memory repair` | Checks the store in full and, if it is damaged, backs it up and rebuilds its indexes ([repair](storage.md#checking-and-repairing-the-store)). `--db PATH` names another store. |
| `user-memory reindex` | Rebuilds the keyword index and embeds every memory that lacks a current vector. |
| `user-memory embedding set --url URL --model NAME` | Chooses the embedding model for semantic search: checks it, calibrates it, saves it and embeds every memory again. `--key-file FILE`, `--dimensions N` and `--allow-http` as in [embedding settings](configuration.md#embedding-settings). |
| `user-memory embedding sync` | Checks, calibrates and re-embeds with the setting in effect. On the installer's Docker server's machine, it runs in the container. |
| `user-memory embedding show` | Shows the endpoint, model, vector length and how duplicates are detected. On the installer's Docker server's machine, it shows the setting from `embedding.env`, then asks the container for the rest. On a client of another server, `show` and `sync` name the command to run on that server's machine. |
| `user-memory embedding off` | Keyword search only; stored vectors are dropped. |

## Run by the clients

Setup registers these with your agents; you rarely run them yourself.

| Command | Effect |
|---|---|
| `user-memory serve` | The stdio MCP server an agent starts. |
| `user-memory host` | The authenticated HTTP server ([server options](configuration.md#server-options)). |
| `user-memory connect` | The stdio connector an agent starts for a remote server. |
| `user-memory session-start --client claude` | The session-start hook (`--client codex` for Codex); see [session hooks](clients.md#session-hooks). |
| `user-memory prompt-submit --client claude` | The hook before each prompt. |
