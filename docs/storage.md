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
version 5), `store_changes` and `memory_changes` (the store's count of its
own changes, since schema version 10: the store's id, the number of its
latest change, and for each memory changed since the number of its latest
change, with the time it was removed for a forgotten one; no text), and
derived search tables that `reindex` can always rebuild: the
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

## Removed text

Text a forget, a correction or an import that takes the file's side removes
leaves the store's files with the change, apart from what a correction or
an import keeps in the memory's [history](memory-model.md#history) of the
values it replaced. Forgetting the memory, `update --drop-history` and the
history's yearly pruning remove those the same way:

- every connection sets `PRAGMA secure_delete`, so SQLite overwrites deleted
  and replaced content with zeros instead of leaving it in free space;
- the keyword index stores its words (as hex stems, never the text) and
  has FTS5's `secure-delete` set, so a deleted entry's words leave its
  pages in the same transaction; `repair` sets it again on an index that
  lost it;
- after the commit, the log is copied into the store and emptied
  (`wal_checkpoint(TRUNCATE)`), so no earlier version of a page stays in
  `<store>-wal`. It waits at most a second for readers; a log another
  connection is still reading stays until the next such write, and a note
  says so on stderr.

A backup made after holds none of the removed text; one made before keeps
it until it is deleted ([privacy](privacy.md)). A store written before this
version can hold text forgotten then in free space inside its pages, which
no count shows, so setup and `user-memory migrate` (which upgrades the
installer's Docker server's store) rewrite a store with memories after its
automatic backup (`VACUUM`). A store without memories isn't rewritten. When
another writer holds the store past the usual 30 seconds, the rewrite is
skipped with a note, and the next setup or migrate tries again.

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

Each write that changes the store, still holding its turn, writes the
store's id and the number of its latest change to `<store>.changed` beside
it (owner-only, a few bytes). A session hook reads that file to tell whether
anything changed since it last looked, without opening the store. A writer
that finds the file missing or naming other numbers than the store holds,
as after a restore or a copy, gives the store a new id first, so every
session reads it whole once. A session keeps where the store stood as the
store gave it: a position holding a digest of the store's id and the
number of the change, never the id itself, which the hook sends back as it
is. A position the store does not know, from another store, a restored one
or another version, gets every memory. Before each prompt, a session on
this machine whose snapshot's position matches the file reads nothing from
the store; one that does not reads only the memories changed since. A program other than user-memory
that edits the store leaves the file as it was, so sessions see that edit
after the next write by user-memory or the next session start, which sets
the file right when it no longer matches the store. A forgotten memory's row in `memory_changes`
keeps its id and the time it went for 30 days, as long as an unused session
file lasts, and is dropped as an automatic backup is made.

## The keyword index

Search finds the memories holding a word of the question through an index of
each memory's words, stemmed as search compares them, so it does not read
every memory to answer. Its answer is the one reading every memory would
give.

- `search_keys` gives each memory a number, in the order memories were
  stored, and copies the columns search filters on.
- `search_words` (SQLite FTS5) holds each memory in rows numbered from
  its number: one for its category, title, type, reason and evidence, and
  one for each [passage](memory-model.md#passages) with the passage's place
  in the text (`start`, `length`). A row holds stems, each at the strongest
  field holding it there, and literal anchors. It stores what it indexes,
  as hex, never the text, so FTS5's `secure-delete` can take a deleted
  memory's words out of its pages at once ([removed text](#removed-text)).
- Triggers on `memories` keep the numbers and filter columns in step with
  every write, from user-memory or any other program, and queue a changed
  memory in `search_pending`. Every write by user-memory stems what it
  queued before it commits; a search first stems what another program left
  queued. An update of `memories` with any conflict clause (`OR ABORT`,
  `OR FAIL`, `OR ROLLBACK`, `OR IGNORE`, `OR REPLACE`) queues the memory
  once and records its history like any other update. To change a memory,
  update it; replacing its row (`INSERT OR REPLACE`, `REPLACE INTO`) skips
  what its removal would do.
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
   never replacing a file) and prints the path. A second backup in the same
   second, such as a migration run again after one rolled back, ends in
   `-2.bak`, the next `-3.bak`;
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
`<store>-wal`, `<store>-shm` and `<store>.changed`, and copy the backup over the store: `backup
list` prints both commands, for PowerShell on Windows. As when [undoing an upgrade](upgrade.md#roll-back-an-upgrade), a leftover
log would be applied to the restored store.

## Checking and repairing the store

`doctor` runs SQLite's full integrity check (`PRAGMA integrity_check`), which
also compares every index with its table and checks the keyword index's
FTS5 structure, then stems every memory again and compares the result with
the keyword index, and checks that the keyword index removes deleted words
at once. Two processes writing the store at once without SQLite's locks
leave exactly that damage: an index whose entries no longer match the
memories. It also checks that no write left behind the row naming its
client in `memory_writer`: every write clears it before committing, and a
row left by another program would name that program's writes.

`user-memory repair` (`--db PATH` for another store; on a server,
`docker compose exec memory user-memory repair`) checks the store the same way
and, when it finds problems:

1. copies the store page by page, damage included, to
   `<store>.repair-<UTC time>.bak` beside it (mode 0600, never replacing a
   file; `-2.bak` for a second in the same second, as for a migration), and
   checks that the copy holds every memory;
2. under the same write lock, clears a `memory_writer` row left behind,
   rebuilds every index from the memories (`REINDEX`) and the keyword index
   from the memories' text (before schema version 6, FTS5 `rebuild`);
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
{"e0000001-0000-4000-8000-000000000001": "Ask before every git push"}
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
refuse an older store with the first message below, and a newer one with the
second, which is what sessions opened before an upgrade show:

```output
<store> has schema version <found>; run the installer again (or ./setup, or `user-memory migrate`) to upgrade it to version <current>. A backup is made first; the store was left unchanged.
<store> has schema version <found>, but this user-memory is older than the store (it uses version <current>); update user-memory and restart your client. The store was left unchanged.
```

Refused stores are left untouched. To undo an upgrade, stop your clients and
every other process using the store, delete `<store>-wal` and `<store>-shm` if
they are there, copy the `.bak` back and install the previous version;
memories recorded since are lost unless exported first. A `-wal` left beside
the store is applied to whatever store sits there, so skipping the delete
brings writes made after the backup into the restored store. The previous
version reads a WAL store as it is. Stores from before schema version 2, and
files that are not user-memory stores, are refused: move the file aside and
run setup to start fresh, then import an export made with the version that
created it.

## The state folder

user-memory keeps three kinds of file in its state folder,
`${XDG_STATE_HOME:-~/.local/state}/user-memory/` (`%LOCALAPPDATA%\user-memory\state\`
on Windows), each private to you:

- `sessions/`: what each recent session saw of memory, which the change
  notice compares against ([session hooks](clients.md#session-hooks)). Each
  file is one session's view of one store or server: its name ends with a
  key made from the store's path or the server's address, so a build or
  setup using another store, or a machine moving from its store to a
  server, keeps a snapshot of its own. A snapshot from before 0.9.0 names
  no store and is never read; that session's next prompt starts a new one
  quietly, and the old file goes with the 30-day clean-up. A session whose
  start could not read memory has a `.missed` file beside its snapshot
  until a prompt gives it the index. A `.scope` file beside each snapshot
  repeats the session's project keys, one per line, written with every
  snapshot, so the tool hooks find the session's delivery index without
  reading the snapshot. `seen-<store key>.json` holds, per
  store, the ids and fingerprints of the rules session start last listed,
  and the ids of the memories agents had recorded when you were last told,
  by project key: no memory text or origin. Session start compares against
  it to list new rules ([session hooks](clients.md#session-hooks)); a
  `seen-<store key>.lock` beside it lets two session starts take turns.
  Both go after a year unused: every session start of the store marks them
  used. Deleting them loses nothing: the next
  session start writes them again and lists nothing that time.
- `delivery/`: the delivery index of each store and scope sessions worked
  in on this machine: `<store key>-<scope key>.index`, a copy of the
  keyword index's rows for the global memories and the scope's projects
  (every word's stem in hex and every literal anchor, with each passage's
  place, no sentence), built at session start, and at the next prompt when
  the session's scope has none it can use, as after a `link`;
  `<store key>-<scope key>.overlay`, the memories changed since, which the
  tool hooks keep current; `<store key>.lock`, which every writer of the store's files
  holds; and at times `<store key>.void`, naming files to take as absent. A
  forget, an update or an import that removes or replaces a memory's text
  builds the files that held it again, under the lock, before it replies.
  `migrate` and `project move` delete the store's files, and the next
  prompt builds them again. A file not written for a day goes at that
  store's next session start.
  Deleting the folder loses nothing: the next session start builds it
  again.
- `servers/`: for each memory server this machine is a client of, the
  server's position as its connector last found it (`<key>.changed`, the
  key a hash of the server's address), when a hook last ran
  (`<key>.wanted`) and when a connector last started to ask
  (`<key>.polling`, which one connector at a time takes), all empty or a
  few bytes. A prompt whose snapshot
  holds that position asks the server nothing
  ([session hooks](clients.md#session-hooks)).
- `instructions/`: one record per instruction file user-memory wrote, with
  its path, time, size and the SHA-256 of its text, so the next write knows
  nobody has saved the file since
  ([instruction files](clients.md#instruction-files)).

All are only working state. Deleting the folder loses no memory; the next
session start takes a new snapshot, and the next write waits for the file
to go quiet. Uninstall removes the folder.

### A delivery file another program holds

On Windows, a virus scanner or a backup tool can hold a delivery file open
so that it can be neither replaced nor deleted. When that happens to a
file holding words a forget, an update or an import removed, the void mark
written before the change names it, so recall skips it, and the reply lists
it in `delivery_left` with this line in `next`:

```output
The files in delivery_left still hold the removed words because another program has them open. Readers skip them; user-memory deletes them at its next write, and `user-memory doctor` deletes them at once.
```

`user-memory doctor` deletes such files, and any a writer killed while
writing left, and says how many:

```output
Delivery index: deleted 1 file that held removed words.
Delivery index: deleted <n> files that held removed words.
```

A file still held is an action, one line per file:

```output
Action needed: Delivery index: <path> still holds removed words because another program has it open; close that program, then run `user-memory doctor` again
```

If another user-memory process keeps the delivery index's lock for 10 s,
doctor fails with a line such as:

```output
Delivery index: user-memory process <pid> has held this machine's delivery index for 10 s
```
