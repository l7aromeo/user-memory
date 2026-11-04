# Storage

The store on disk: its tables, the write-ahead log, how writers take turns,
schema versions, automatic backups, repair, and the files user-memory keeps
beside it and in its state folder.

- [Tables](#tables)
- [Write-ahead log](#write-ahead-log)
- [Writers take turns](#writers-take-turns)
- [The keyword index](#the-keyword-index)
- [Schema versions](#schema-versions)
- [Automatic backups](#automatic-backups)
- [Checking and repairing the store](#checking-and-repairing-the-store)
- [Titles for older memories](#titles-for-older-memories)
- [The state folder](#the-state-folder)

## Tables

The store is one SQLite database: the `memories` table, `memory_origins`,
`memory_distinct`, `conversation_projects` (each conversation's
[links](tools.md#conversation-links)), `embedding_cutoffs` (cutoffs computed for an
embedding model and vector length without built-in ones, since schema
version 5), and derived search tables that `reindex` can always rebuild: the
[keyword index](#the-keyword-index) (`search_keys`, `search_words`,
`search_pending`, `search_state` and `search_changes`, since schema version
6) and
`memory_vectors` for embeddings. Memory IDs are UUID strings; insertion order
tracks recency.

## Write-ahead log

The store runs in SQLite's write-ahead log (WAL) mode, so readers, such as
each session start's instruction sync, never make a writer wait. Recent writes
sit in `<store>-wal` beside the store until SQLite copies them into it, and
`<store>-shm` coordinates the processes using it; both take the store's mode
and are removed when the last process closes the store. The mode is kept in
the file: the first open by this version converts a store, and a store busy
at that moment keeps the rollback journal until a later open. WAL needs a
local disk; on a network filesystem (NFS, SMB) processes on different machines
cannot share the log. `doctor` reports the mode. Backups that `migrate`,
`repair` and `project move` make are copied through SQLite's online backup,
so they include writes still in the log, and are one whole file each, in the
rollback journal mode.

## Writers take turns

Writers take turns. Within a process they queue for the store; across
processes, such as one server per agent session plus the command line, each
writer first takes an exclusive lock on `<store>.lock` beside the store
(`flock` on Unix, `LockFileEx` on Windows; owner-only, created with it and
kept), and releases it when its transaction ends. No order is promised,
within a process or across processes. A writer waits at most 30 seconds at
each stage: its process's turn, the store's lock file, and SQLite's own lock
(its busy timeout). Each commit waits until the disk has its data, which
keeps every write durable and on a slow, busy disk can take seconds. Past
the 30 seconds a write fails with "database is locked: another process kept
its write turn on the store for 30 s", or, when it is still queued behind
another write of its own process, "database is locked: another write in this
process kept its write turn on the store for 30 s". Once the log passes
about 4 MB, the commit that finds it so copies it into the store after
passing its turn on, so that copy never holds up other writers; SQLite's own
copy inside the commit is off. A copy that fails is reported on stderr and
retried by the next commit; the write it followed has succeeded. Readers
take no lock. A server does its store work apart from the threads that read
requests and send replies, so while its writes wait it goes on answering
other calls, reads at once. The lock file holds nothing: restoring a backup
leaves it as it is, and uninstall with `--purge` deletes it with the store.

## The keyword index

Search finds the memories holding a word of the question through an index of
each memory's words, stemmed as search compares them, so it does not read
every memory to answer. Its answer is the one reading every memory would
give.

- `search_keys` gives each memory a number, in the order memories were
  stored, and copies the columns search filters on.
- `search_words` (SQLite FTS5, without a copy of the text) holds each
  memory's stems under its number, each at the strongest field holding it:
  category, title, type, or the text.
- Triggers on `memories` keep the numbers and filter columns in step with
  every write, from user-memory or any other program, and queue a changed
  memory in `search_pending`. Every write by user-memory stems what it
  queued before it commits; a search first stems what another program left
  queued.
- `search_state` records which stemming made the stored stems. A user-memory
  that stems differently, after an upgrade, does not read an index made by
  another: search reads and stems every memory itself, which is slower on a
  large store, until `migrate` (run by the installer and setup) or
  `reindex` rebuilds the index. `doctor` reports it.
- `search_changes` logs which memories and vectors changed, in order, so a
  long-running server that keeps vectors in memory reloads only those
  ([semantic search](server.md#semantic-search)).
  Writes keep the newest 10,000 changes.

A memory with a word too long for the index (over 16,384 bytes, which
recording refuses) is left unindexed, and search then reads every memory.

## Schema versions

The store records its schema version (`PRAGMA user_version`). When a new
version changes the schema, the installer, `user-memory migrate` or a
checkout's `./setup` ([run from a checkout](dev/development.md#run-from-a-checkout)):

1. copies the store to `<store>.v<OLD>-<UTC time>.bak` beside it (mode 0600,
   never replacing a file) and prints the path;
2. applies every pending migration step in one transaction; if any step fails,
   nothing changes and the backup stays.

A step that needs something only the user can give refuses before step 1,
so a refusal leaves no backup and no change. So does a damaged store, one
that fails SQLite's integrity check: migrating copies its rows, and a
damaged index can hide some, so it asks for `user-memory repair` first.

## Automatic backups

A local store, or a server's, keeps daily copies of itself in a `backups`
folder beside it (mode 0700): `<store dir>/backups/<stem>-<UTC date>.<ext>`,
such as `memories-2026-10-18.sqlite3`, mode 0600. There is no scheduler. The
first write once the newest backup is at least 24 hours old makes one, after
its commit has passed the write turn on, so other writers never wait for it;
only that write's own reply waits for the copy. Every write counts: record,
update, forget, import, links, re-indexing and `project move`. Setup makes
one too when the store has memories, so an upgraded store has one at once.
A store without memories gets none.

A server (`serve` for each agent session, `host` for a shared server) also
checks, on a thread of its own beside its work: when it starts, every hour
while it runs (`CHECK_EVERY`), and as it stops, on its input closing, Ctrl-C
or, on Unix, SIGTERM. A check makes a backup when the store or its log was
written after the newest backup and either that backup is 24 hours old or
the store has had no change for an hour (`QUIET`); the check as it stops
needs no quiet hour, since no server may look again for days. A backup made
the same day replaces that day's file, so it uses no rotation slot. A store
unchanged since its newest backup gets none, so copies of the same memories
don't rotate distinct older backups out. Each backup first copies the log
into the store (a passive checkpoint), so the checkpoint as the last
connection closes has nothing left to write and the store doesn't look
changed; only a log another process was still reading at the backup can
cause one copy of the same memories again. The server closes its connection
before that last check, and waits for it at most 5 seconds
(`SHUTDOWN_WAIT`); a copy cut short is left as a partial file. Several servers of one store may check at once: each copy
is renamed into place, so the newest wins.

Each copy is taken like migrate's: SQLite's online backup from a read-only
connection, so it includes writes still in the log, into
`.<name>.partial-<process id>` in the folder, converted to the rollback
journal (one whole file), then renamed into place, so a copy cut short never
looks like a backup. Copies cut short more than a day ago are deleted.
`backup now` makes one at once and replaces that day's.

The newest 7 are kept (`KEEP` in `src/memory/backups.rs`); after a new one is
written, older ones are deleted. Migrate, repair and project move backups
(`.bak` beside the store) are a separate kind and never rotated.

A backup that fails never fails the write. The write prints
``user-memory: the automatic backup of <store> failed: …`` to stderr, writes
the reason to `backups/last-failure`, and that process does not try again for
an hour; the next backup that succeeds removes the note. `doctor` prints the
newest backup and its age, and an Action needed step when a store with
memories has none, when the newest is over 48 hours old though the store or
its log changed since, or when `last-failure` is there. Uninstall's `--purge`
deletes the backups, the note and copies cut short with the store, then the
folder if nothing else is in it; without `--purge` they stay.

To restore one, stop every client and server using the store, delete
`<store>-wal` and `<store>-shm`, and copy the backup over the store: `backup
list` prints both commands, for PowerShell on Windows. As when [undoing an upgrade](upgrade.md#roll-back-an-upgrade), a leftover
log would be applied to the restored store.

## Checking and repairing the store

`doctor` runs SQLite's full integrity check (`PRAGMA integrity_check`), which
also compares every index with its table and checks the keyword index's
FTS5 structure, then stems every memory again and compares the result with
the keyword index. Two processes writing the store at once without SQLite's
locks leave exactly that damage: an index whose entries no longer match the
memories.

`user-memory repair` (`--db PATH` for another store; on a server,
`docker compose exec memory user-memory repair`) checks the store the same way
and, when it finds problems:

1. copies the store page by page, damage included, to
   `<store>.repair-<UTC time>.bak` beside it (mode 0600, never replacing a
   file), and checks that the copy holds every memory;
2. under the same write lock, rebuilds every index from the memories
   (`REINDEX`) and the keyword index from the memories' text (before schema
   version 6, FTS5 `rebuild`);
3. if the store still fails the check, rewrites the whole file (`VACUUM`),
   which mends damage beyond the indexes, such as a miscounted free-page list;
4. counts the memories again, without any index, and prints what it found.

Memories themselves are never changed. A healthy store is reported clean and
left alone, with no backup. If a memory went missing or the store is still
damaged, repair fails and says so, with the backup beside it; restore a backup
(as when [undoing an upgrade](upgrade.md#roll-back-an-upgrade)) or an export you trust. Repair works on a store from an earlier schema version
too, so a damaged store can be repaired before it is migrated.

## Titles for older memories

Schema version 3 gives every memory a title. A title decides how agents
understand the memory, so nothing derives one: the upgrade from version 2
(`./setup --titles FILE`, `user-memory migrate --titles FILE`, `-` for stdin)
takes each title from a JSON object mapping every memory id to the title the
user approved:

```json
{"68da18b4-5d40-4746-930f-22a7c859e8f7": "Push, publish or tag only with the user's explicit go-ahead"}
```

Without the map, with an id missing or unknown, or with a title that is
empty or over 120 characters, the upgrade changes nothing
and lists the untitled memories with their content, so an agent can propose
titles for the user to approve (the user-memory skill's upgrade playbook walks
through it). Each title is stored as written, its whitespace tidied (trimmed,
with runs and line breaks made one space), as any title is. A map given when the
store needs none is reported as not needed. Until the store is upgraded, the
server and hooks refuse it, so memory is unavailable on that store.

Nothing else migrates. The MCP server, the CLI, the session hook and `host`
refuse an older store with "run the installer again (or ./setup, or
`user-memory migrate`) to upgrade it", and a newer one with "update user-memory and restart your client" (what
sessions opened before an upgrade show). Refused stores are left untouched. To
undo an upgrade, stop your clients and every other process using the store,
delete `<store>-wal` and `<store>-shm` if they are there, copy the `.bak` back
and install the previous version; memories recorded since are lost unless
exported first. A `-wal` left beside the store is applied to whatever store
sits there, so skipping the delete brings writes made after the backup into
the restored store. The previous version reads a WAL store as it is. Stores
from before schema version 2, and files that are not user-memory stores, are
refused: move the file aside and run setup to start fresh, then import an
export made with the version that created it.

## The state folder

user-memory keeps two kinds of file in its state folder,
`${XDG_STATE_HOME:-~/.local/state}/user-memory/` (`%LOCALAPPDATA%\user-memory\state\`
on Windows), each private to you:

- `sessions/`: what each recent session saw of memory, which the change
  notice compares against ([session hooks](clients.md#session-hooks)). Each
  file is one session's view of one store or server: its name ends with a
  key made from the store's path or the server's address, so a build or
  setup using another store, or a machine moving from its store to a
  server, keeps a snapshot of its own. A snapshot from before 0.9.0 names
  no store and is never read; that session's next prompt starts a new one
  quietly, and the old file goes with the 30-day clean-up.
- `instructions/`: one record per instruction file user-memory wrote, with
  its path, time, size and the SHA-256 of its text, so the next write knows
  nobody has saved the file since
  ([instruction files](clients.md#instruction-files)).

Both are only working state. Deleting the folder loses no memory; the next
session start takes a new snapshot, and the next write waits for the file
to go quiet. Uninstall removes the folder.
