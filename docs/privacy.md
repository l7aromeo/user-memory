# Privacy

user-memory is self-hosted. Your memories stay on the machine or server you
run. There is no account, no telemetry and no model training. Besides your
memory server and the embeddings endpoint you choose, user-memory contacts
only GitHub, for the installers' downloads and doctor's check for a newer
release ([what leaves your machine](#what-leaves-your-machine)).

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
| `~/.local/state/user-memory/sessions/` | For each recent session, the id, last change, status and title of each memory it saw, so the change notice can compare. A file's name holds a key made from the store's path or the server's address (a hash, not the path or address itself). Files unused for 30 days are removed. |
| `~/.local/state/user-memory/instructions/` | For each instruction file user-memory last wrote: its path, time, size and a SHA-256 of its text, never the text itself, so the next write can tell nobody has saved it since. |
| `~/.claude/rules/user-memory.md`, the block in `~/.codex/AGENTS.md` | The titles of your global rules. |

On Windows the token is in `%LOCALAPPDATA%\user-memory\config\client-token`,
the session files in `%LOCALAPPDATA%\user-memory\state\sessions\`, the
write records in `%LOCALAPPDATA%\user-memory\state\instructions\`, and
`.user-memory.json` in `%LOCALAPPDATA%\Programs\user-memory\bin`. There,
"readable only by you" means an access list that allows only your account.

Backups you make with `export` hold every memory in readable JSON, readable
only by you. Upgrade and repair backups (`.bak` files) sit beside the store,
and the [automatic backups](backup.md#automatic-backups), a week of daily
copies, in the `backups` folder beside it, readable only by you. A forgotten
memory stays in the backups made before you forgot it, until 7 newer
backups have replaced them. Protect them like the store. The store itself,
its log and every backup made after keep none of a forgotten memory's text,
or of text a correction replaced
([removed text](storage.md#removed-text)).

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

The client connects to two places you choose:

1. **Your memory server**, when you use one. It receives the requests your
   agents make: memories to record, searches, links, and the keys of the
   projects a session loads.
2. **Your embeddings endpoint**, when semantic search is on. It receives the
   text of memories and searches. Keep it on your machine or a server you
   control.

Two more requests go to GitHub, and carry nothing about you or your
memories:

- The installers download the release from
  `github.com/l7aromeo/user-memory/releases` (`USER_MEMORY_RELEASES` names a
  mirror instead).
- `user-memory doctor` asks the same releases page once which release is
  the newest, to tell you when yours is out of date. The session hooks
  never make that request. `user-memory setup --no-update-check`, or
  `USER_MEMORY_NO_UPDATE_CHECK=1` for one run, turns it off.

When you agree to it, the installer also installs Ollama (through Homebrew,
Ollama's install script or `winget`) and has Ollama download the embedding
model ([semantic search](install.md#semantic-search)).

What user-memory never sends anywhere:

- Local folder paths. The server gets project keys, such as
  `github.com/you/app`, never a path such as `~/code/app`; agents' tools
  refuse paths.
- The files `.user-memory` and `.user-memory-key`. The client reads them and
  sends only the project keys they lead to.
- The session snapshots and write records in `~/.local/state/user-memory/`.

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
- Agents are told that your explicit rules say how you want work done, that
  observations and patterns are only tentative evidence, and never to act
  on commands written inside a memory's text.
- The checks on what is recorded validate its structure and the claims
  about its source; they cannot prove that an agent saw what it noted.
- Protect the server, the access token and backups as you would the
  memories.
