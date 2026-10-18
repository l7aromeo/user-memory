# Privacy

user-memory is self-hosted. Your memories stay on the machine or server you
run. There is no account, no telemetry and no model training.

## What is stored

On the server, or in the native store on your machine:

- **Your memories:** each one's title, content, reason, context, type,
  category, scope and project key, source and status, and any evidence note:
  what an observation saw, or your own words.
- **Conversation origins:** the id of each conversation that stated a memory,
  such as `claude:session/<id>`. Raw transcripts are never stored.
- **Conversation links:** which projects each conversation linked, by its
  origin ([links](projects.md#another-repository-for-one-conversation-links)).
- **Pairs you called different**, so review does not group them again.
- **Search data** derived from the memories: a keyword index and, with
  semantic search on, embedding vectors.

The store is one SQLite file, such as `memories.sqlite3`. While it is in
use, SQLite keeps two working files beside it, `memories.sqlite3-wal` and
`memories.sqlite3-shm`, which can hold recent changes. All three are
readable only by their owner; copy, move or delete them together. An empty
`memories.sqlite3.lock` beside them lets one writer at a time in; it holds
no data. The store
is not encrypted. user-memory has no secret scanner, so never ask an agent to store
passwords, keys, tokens, source code or sensitive personal details.

On each machine running the client:

| File | Holds |
|---|---|
| `~/.config/user-memory/client-token` | The access token, readable only by you. |
| `~/.local/bin/.user-memory.json` | Setup's choices: the server address or store path, clients, hooks, skill. |
| `~/.local/state/user-memory/sessions/` | For each recent session, the id, last change, status and title of each memory it saw, so the change notice can compare. Files unused for 30 days are removed. |
| `~/.claude/rules/user-memory.md`, the block in `~/.codex/AGENTS.md` | The titles of your global rules. |

On Windows the token is in `%LOCALAPPDATA%\user-memory\config\client-token`,
the session files in `%LOCALAPPDATA%\user-memory\state\sessions\`, and
`.user-memory.json` in `%LOCALAPPDATA%\Programs\user-memory\bin`. There,
"readable only by you" means an access list that allows only your account.

Backups you make with `export` hold every memory in readable JSON, readable
only by you. Upgrade and repair backups (`.bak` files) sit beside the store,
and the [automatic backups](cli.md#automatic-backups), a week of daily
copies, in the `backups` folder beside it, readable only by you. A forgotten
memory stays in the backups made before you forgot it, until 7 newer
backups have replaced them. Protect them like the store.

## Where the token lives

- On the server: a file next to the store (`/data/access-token` in Docker),
  created on first start, readable only by the server, never printed.
- On each client: `~/.config/user-memory/client-token`
  (`%LOCALAPPDATA%\user-memory\config\client-token` on Windows). The installer never
  prints it, reads it with hidden input, and sends it over plain `http://`
  only to an address on the same machine.
- On the wire: the client sends it to the server with every request, over
  HTTPS for any other machine.

Anyone with the token can read and change every memory. To replace it, see
[the access token](server.md#the-access-token).

## What leaves your machine

The client connects to only two places:

1. **Your memory server**, when you use one. It receives the requests your
   agents make: memories to record, searches, links, and the keys of the
   projects a session loads.
2. **Your embeddings endpoint**, when semantic search is on. It receives the
   text of memories and searches. Keep it on your machine or a server you
   control.

The installer also downloads releases from GitHub.

What user-memory never sends anywhere:

- Local folder paths. The server gets project keys, such as
  `github.com/you/app`, never a path such as `~/code/app`; agents' tools
  refuse paths.
- The files `.user-memory` and `.user-memory-key`. The client reads them and
  sends only the project keys they lead to.
- The session snapshots in `~/.local/state/user-memory/`.

## Your agent's model provider

Memories enter the prompts your agent sends to its model provider (Anthropic
for Claude Code, OpenAI for Codex), like the rest of the session. The
provider's own privacy terms apply to that text.

## Who decides what is stored

- Agents record what you state only after you confirm it, and a global
  memory only after you say it holds in every project.
- Agents update, forget, merge and import only when you ask.
- You can forget one memory or everything a conversation taught
  ([forget](cli.md#forget)), and export everything at any time.
