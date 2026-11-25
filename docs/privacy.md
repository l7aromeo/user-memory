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
- **Which memories each conversation used:** for each project's newest 40
  conversations, by origin, the ids of the memories it read, recorded or
  was shown, whether it held to or set aside one, and when; no text. They
  order the index and choose session start's
  [most-used part](how-it-works.md#the-index-at-session-start). Forgetting
  a memory deletes its rows with it. A client with a read-only token
  sends none, so its conversations never count.
- **Pairs you called different**, so review does not group them again.
- **Search data** derived from the memories: a keyword index and, with
  semantic search on, embedding vectors.

The store is one SQLite file, such as `memories.sqlite3`. While it is in
use, SQLite keeps two working files beside it, `memories.sqlite3-wal` and
`memories.sqlite3-shm`, which can hold recent changes. All three are
readable only by their owner; copy, move or delete them together. An empty
`memories.sqlite3.lock` beside them lets one writer at a time in; it holds
no data. `memories.sqlite3.changed` holds the store's id and the number of
its latest change, no text. The store is not encrypted. user-memory has no secret scanner, so
never ask an agent to store passwords, keys, tokens, source code or
sensitive personal details.

Owner-only files don't help when a disk is lost or stolen, so turn on
full-disk or volume encryption on every machine that holds the store or its
backups: FileVault on macOS, BitLocker on Windows (Device Encryption on
Windows Home), LUKS on Linux. For the Docker server, that is the host's disk
under the `user-memory-data` volume; on Docker Desktop, the host's disk
holds the VM disk image the volume lives in. Exports and backup copies that
leave the machine are plain data, which user-memory doesn't encrypt:
[encrypt them first](backup.md#copies-that-leave-the-machine).

On each machine running the client:

| File | Holds |
|---|---|
| `~/.config/user-memory/client-token` | The access token, readable only by you. |
| `~/.local/bin/.user-memory.json` | Setup's choices: the server address or store path, clients, hooks, skill. |
| `~/.local/state/user-memory/sessions/` | For each recent session, the id, last change, status and title of each memory it saw, so the change notice can compare, and in a second file the keys of its projects, which the first already holds, for the tool hooks. A file's name holds a key made from the store's path or the server's address (a hash, not the path or address itself). Files unused for 30 days are removed. |
| `~/.local/state/user-memory/sessions/*.recall` | For each recent session, beside its snapshot, what [recall during the task](how-it-works.md#recall-during-the-task) needs: for each agent, its last 30 steps (a command line, a path, a prompt, a subagent's task) as terms, each word as its stem in plain text, which reads back as the word, and the anchors (commands, paths, code spans) as written, lowercased, but no sentence, and never a tool's result or a file's contents; the tasks of subagents not yet started, as the same terms; and the ids of the memories pushed, with the passage and the step each was pushed at. Readable only by you. Emptied at each new session; removed, with its `-wal` and `-shm` files, once not written for 30 days. |
| `~/.local/state/user-memory/handover-*.json` | Only while an open session moves to a new binary after an upgrade: the session's handshake, the tools it was given and the input read but not yet answered, which can hold a memory being recorded. Readable only by you. The new binary deletes it as it starts; if a dying process leaves one behind, the next session to start deletes it once it is ten minutes old. |
| `~/.local/state/user-memory/delivery/` | For each store on this machine and each set of projects a session worked in, the delivery index: every word of the global memories and those projects' memories as a stem in hex, which decodes back to the word, and their literal anchors (code spans, paths, names) as written, with each passage's place, but no sentence. Beside it, the changes since, a lock and, from the first forget, update or import that removes a memory's text, the names of files to ignore, a file left empty when none is. For a memory server's store, the same for every memory the server lets this machine's sessions see in their projects, downloaded from the server; no sentence, title or passage text either. File names hold hashes of the store's path or the server's address and of the project keys. A file not written for a day is deleted at the next session start of that store, and by `user-memory doctor` for a server's. |
| `~/.local/state/user-memory/servers/` | For each memory server this machine uses, the server's position in its count of changes (an opaque string, not its id) as the connector last polled it, and the times a hook last ran and a poll last started, with the id of the connector process polling; a lock a connector holds while it takes the poll; and a lock each running connector holds, so the hooks know one runs. A file's name is a hash of the server's address. |
| `~/.local/state/user-memory/instructions/` | For each instruction file user-memory last wrote: its path, time, size and a SHA-256 of its text, never the text itself, so the next write can tell nobody has saved it since. |
| `~/.claude/rules/user-memory.md`, the block in `~/.codex/AGENTS.md` | The titles of your global rules. |

On Windows the token is in `%LOCALAPPDATA%\user-memory\config\client-token`,
the session and recall files in `%LOCALAPPDATA%\user-memory\state\sessions\`, the
delivery index in `%LOCALAPPDATA%\user-memory\state\delivery\`, the
hand-over files in `%LOCALAPPDATA%\user-memory\state\`, the
write records in `%LOCALAPPDATA%\user-memory\state\instructions\`, and
`.user-memory.json` in `%LOCALAPPDATA%\Programs\user-memory\bin`. There,
"readable only by you" means an access list that allows only your account.

Backups you make with `export` hold every memory in readable JSON, readable
only by you. Upgrade and repair backups (`.bak` files) sit beside the store,
and the [automatic backups](backup.md#automatic-backups), a week of daily
copies, in the `backups` folder beside it, readable only by you. Protect
them like the store.

### What a forgotten memory leaves

`forget` removes a memory's text from the store, its log and every backup
made after ([removed text](storage.md#removed-text)), and from this
machine's delivery index before it replies, and reduces its history to
tombstones, each change's operation and time. If another user-memory
process holds the delivery index for 10 s, the forget changes nothing and
names that process. A delivery file another program, such as a virus scanner
or a backup tool, holds open can be neither replaced nor deleted: the
reply lists it in `delivery_left`, readers skip it, and the next write or
`user-memory doctor` deletes it.

On a memory server, a forget or a correction can't reach the client
machines' delivery index itself. Each client's connector takes the words
out: at its next poll, within 5 seconds while a session there is active
and within a minute on a quiet machine, it asks the server what changed
and writes the files again without them. Until then a hook there may
still match those words. The hooks read a server's files only while a
connector runs on the machine and has heard from the server in the last
three minutes, and only once the files hold every change the connector
has heard of. A client whose connector doesn't run keeps the files,
unread, until its next connection, whose first poll takes the words out
before any hook reads them; its next session start or `user-memory
doctor` once they are a day old; or uninstall.

A correction keeps the text it replaced in the
memory's [history](memory-model.md#history) for a year, then drops it as an
automatic backup is made; `update --drop-history` drops it at once, with
the rest of that memory's history. Copies made before keep it, and
restoring one of them brings the memory back:

- the automatic backups made before the forget, until 7 newer backups have
  replaced them;
- the copy a forget by origin or by import makes just before it deletes memories, beside
  the automatic backups, which goes 7 days after it was made;
- the `.bak` files beside the store, from upgrades, repairs and project
  moves, which are never rotated;
- every export made before the forget, wherever you keep it;
- the session files on each machine, which keep the memory's title until
  they go unused for 30 days;
- the store's count of its changes, which keeps the memory's id and the time
  it was forgotten, no text, for 30 days, so running sessions learn it went;
- the instruction files on your other machines, which keep a forgotten
  global rule's title until their next session start (the machine that
  forgot it updates its own at once);
- a non-local embeddings endpoint, which received the memory's text when it
  was embedded and may have kept it, as its provider decides
  ([what leaves your machine](#what-leaves-your-machine)).

To remove every copy, see [forget](cli.md#forget).

## Where the token lives

- On the server: the shared token file next to the store
  (`/data/access-token` in Docker), created on first start, readable only by
  the server, never printed; and for each machine's own token only its
  SHA-256, in `accounts.sqlite3` beside the store, with its name, scope,
  creation, last use (to the hour) and expiry.
- On each client: `~/.config/user-memory/client-token`
  (`%LOCALAPPDATA%\user-memory\config\client-token` on Windows). The installer never
  prints it, reads it with hidden input, and sends it over plain `http://`
  only to an address on the same machine.
- On the wire: the client sends it to the server with every request, over
  HTTPS for any other machine.

Anyone with a token that can write can read and change every memory; a
read-only token can read every memory. To give each machine its own token,
revoke one or replace the shared one, see
[access tokens](server.md#access-tokens).

## What leaves your machine

The client connects to two places you choose:

1. **Your memory server**, when you use one. It receives the requests your
   agents make: memories to record, searches, links, the keys of the
   projects a session loads, and which memories each session used.
2. **Your embeddings endpoint**, when semantic search is on. It receives the
   text of memories and searches. Keep it on your machine or a server you
   control. When it isn't on this machine (a loopback address, or the host
   gateway through which the installer's Docker server reaches this
   machine), `user-memory embedding set`, `user-memory embedding show` and
   `user-memory doctor` name its host:
   ``Semantic search sends memory text and searches to api.example.com``.

Two more requests go to GitHub, and carry nothing about you or your
memories:

- The installers and `user-memory upgrade` download the release from
  `github.com/l7aromeo/user-memory/releases` (`USER_MEMORY_RELEASES` names a
  mirror instead, which `upgrade` names before it asks).
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
- The session snapshots, the rules each store last showed (ids and
  fingerprints only) and the write records in `~/.local/state/user-memory/`.

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
