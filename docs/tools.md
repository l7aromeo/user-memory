# MCP tools

The tools agents call, their filters, search, the index and paging. The
[command line](cli.md) runs the same operations.

- [MCP tools](#mcp-tools)
- [Replies and errors](#replies-and-errors)
- [Scope and filters](#scope-and-filters)
- [Search](#search)
- [The index](#the-index)
- [Pages](#pages)
- [Conversation links](#conversation-links)
- [Consolidation review](#consolidation-review)

## MCP tools

| Tool | Purpose |
|---|---|
| `index` | Every memory in scope, one line each (8-character id and title): rules first by category, then other memories by type; optional `topic` puts task-relevant memories first in their groups; [filters](#scope-and-filters); paged |
| `read` | Whole memories by up to 20 ids (full or the index's 8 characters) or by category; unknown ids and ids outside `status` are reported, and `projects` and `scope` given with ids are ignored; a category read takes the [filters](#scope-and-filters) and is paged; `history: true` adds each memory's [history](memory-model.md#history), and lists a forgotten id's tombstones under `forgotten` |
| `search` | Find relevant memories by topic; [filters](#scope-and-filters); paged |
| `record` | Save a typed explicit memory, grounded observation, or evidence-backed inference; if similar memories exist, nothing is stored until the agent confirms, supersedes or marks it distinct |
| `update` | Correct or confirm a memory on the user's request, or with `distinct_from` mark memories the user calls different; `drop_history` reduces its earlier [history](memory-model.md#history) to tombstones |
| `forget` | Delete one memory, every memory and link from one conversation origin, or undo one import by its name (`import`): delete what it added and restore what it replaced, keeping memories changed since; invalidate dependent inferences; a forget that deletes names the copy of the store it made first (`backup`); `next` says what to do about `staled` and `left_superseded` when either has entries |
| `link` | Keep projects loaded for one conversation origin: its session start and change notices include them, after resume and compaction too; returns the linked projects' index lines; `unlink` removes. See [Conversation links](#conversation-links) |
| `review` | On the user's request only: recent rules and agents' memories, then groups of duplicates, cross-scope copies, overlaps and stale patterns; `keep` marks a group the user leaves as is |
| `export` | On the user's request: write every memory, or those the [filters](memory-model.md#exporting-or-importing-part-of-the-store) select, to a JSON file in the exports directory; returns a summary only, whose `next` says in words, never as a path, whether the file is beside the store on this machine or on the memory server |
| `import` | On the user's request: preview an export file from the exports directory, or the memories the filters select from it; writes only with `apply` and `user_confirmed` after the user approves; `apply` without `user_confirmed` previews, and its reply's `reason` says it was not applied; a preview that applying would carry out says in `next` to show it to the user and wait for their answer; an import's reply holds no memory text except the titles of the rules it adds or replaces, each one line, at most 20 per list (`rules`) |

## Hints

Every tool declares the four MCP tool hints, so an agent app can run the
tools that only read without asking and ask before the ones that change or
remove memories. Each hint is set by what the tool can do at worst:

| Tool | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` |
|---|---|---|---|---|
| `index` | true | false | true | false |
| `search` | true | false | true | false |
| `read` | true | false | true | false |
| `review` | false | false | true | false |
| `link` | false | false | true | false |
| `record` | false | false | false | false |
| `update` | false | true | false | false |
| `forget` | false | true | true | false |
| `import` | false | true | false | false |
| `export` | false | true | false | false |

- `search` and `index` stay read-only although they may save a vector for a
  memory that lacks one: it is made from the memory's own text, and nothing
  the user sees changes.
- `review` writes only with `keep`, and `link` only records the
  conversation's projects; neither changes or removes a memory.
- `record` may supersede a memory, but the old one is kept and marked.
- `update` overwrites fields, `forget` deletes, `import` can replace
  memories with the file's, and `export` with `overwrite` replaces an
  export file.
- `openWorldHint` follows the embedding setting. With an embeddings
  endpoint that isn't on this machine (see
  [What leaves your machine](privacy.md#what-leaves-your-machine)), it is
  true on `index` and `search`, which send the topic and any memory that
  lacks a vector, and on `record`, which sends the new memory for its
  duplicate check. A memory server decides from its own setting. The table
  shows the hints with semantic search off or on this machine.

## Replies and errors

A call that fails, such as an unknown id or a store that cannot be opened,
returns its reason with `isError: true`. A refusal the agent resolves itself,
a `reason` beside a false success field such as `"stored": false`, is a
normal reply. The CLI's memory commands, run against a server, end with that
reason and a failing status, as they do on a local store.

`read` and `search` return each memory without its empty fields: an empty
text such as `reason` or `context`, an empty list such as `origins` or
`evidence_ids`, and a null such as `superseded_by`. A field left out is empty.
Exports keep every field.

A tentative memory, an agent's observation or inference, also carries a
`note`. An inference, or an observation of the user's preferences,
constraints, values, behavior or patterns, which could be taken for what the
user wants, carries ``An agent's observation or inference, not the user's
rule: don't apply it unasked; you may mention it or ask the user, and only
their word makes it a rule.`` An agent's observation of any other type,
such as a fact, decision, goal, lesson or a type of its own like `style`, is
about the work, as only a preference, constraint or value becomes a rule,
and carries ``An agent's note from an earlier session: check it against the
project before relying on it.`` `review` and
the `similar` list of a refused `record` carry the note too. It is not
stored, so exports leave it out.

`read` gives an observed memory seen again in other conversations its
newest two sightings as `observations` (each an `origin` and its
`evidence`), and how many more `history` lists as `observations_more`.

`index` and `link` also accept fields that only user-memory's own client
sends, for its session hooks and instruction files (`stamps`, `instructions`,
`folder_projects` and the like). The tools' published schemas leave them out,
so agents never load or set them.

## Scope and filters

`index`, `search` and `read` (by category) cover the active global memories
plus those of the listed projects, or the global ones only when none is
listed. Another project's memories appear only when it is listed, so a
session in one repository does not see another's exceptions.

- `projects` (CLI `--project`, or `--projects` as the tools name it;
  repeatable) lists project keys or git remote
  URLs. The tools refuse local paths, since the server may run on another
  machine; the CLI resolves a path (`.`, `~/code/repo`, `file://…`) to its
  [project key](projects.md#project-identity) first. The session hook passes its working
  directory's project.
- `scope` widens or narrows that: `global` (global only), `project` (only the
  listed projects') or `all` (every project, for review or inspection).
- `category` (with its synonyms), `type`, `source` (explicit, observed or
  inferred) and `origin` (one conversation) narrow it further.
- `status` is `active` (confirmed and tentative, in force) unless set to
  `ended` (confirmed or tentative, but their
  [condition](memory-model.md#conditions) ended or their date passed),
  `stale`, `superseded` or `all`, which includes ended ones. Each memory
  out of force by its condition in a reply carries `ended`: when, how, the
  evidence and the conversation, or for a passed date that date with how
  `date`. A read by ids takes only `status`: ids outside it are listed
  under `excluded` with their status; under `active` an ended memory is
  still read, marked `ended`. An id names one memory in any
  scope, so `projects` and `scope` given with ids are ignored, and the other
  filters are refused.

Filters apply before [paging](#pages), so a cursor pages through the filtered
result. An agent that works in a second repository during a session
[links](#conversation-links) it; without the session hooks it passes
`projects: [<its git remote>]` to each call.

## Search

Search ranks the memories the [filters](#scope-and-filters) select, active
global ones plus the listed projects' by default. The MCP tool takes the
topic as `topic`, or `query`.

- **Keyword:** every candidate memory is scored. Words are compared by
  their stems, so "releases" finds "release" and "paths" finds "path";
  common function words are ignored. Each word of the question counts by
  the strongest field holding it (category, then title, then type, then the
  text), and by how rare it is among the candidates, so a word most memories
  share adds little. A word written as an anchor (a code span, a path such
  as `build.rs`, an identifier such as `parse_entry`, or a command with its
  subcommand such as `git stash`) and asked for as written counts again.
  The text is read in [passages](memory-model.md#passages): a memory scores
  its category, title, type, reason and evidence plus its best passage, so
  a long memory doesn't outrank a short one by matching in several places.
  Among equal scores, a memory whose negations agree with the question's
  comes first: a word after "not", "no", "never", "without", "nor",
  "cannot" or "n't" in the same clause is negated, and the title decides
  for a word it holds, else the text. "Run the slow tests in CI, not
  before every commit" holds `before` and `commit` negated, so it follows
  an equal match that holds them plainly for "tests to run before a
  commit" and leads for "tests never to run before a commit". A negation
  never changes a score. The negators are English words, so in another
  language ties keep their order by recency; and a lone `t`, as in
  "AT&T", reads as the end of "n't", which can only reorder a tie. Remaining ties, and ties past the first two pages,
  go to the newest. A [keyword index](storage.md#the-keyword-index) of
  every passage's stems finds the matching memories without reading the
  others, with the same result. Category synonyms match at query time:
  programming/coding/software development/development,
  communication/comms/writing style, and workflow/process.
- **Semantic (optional):** an OpenAI-compatible embeddings endpoint finds
  memories phrased differently from the question. Of the 20 closest, those
  near the best match join every keyword match. "Near" means within three
  quarters of the gap between the best and the median similarity of this
  question's candidates, so a far memory is never listed just to fill the
  count. Both rankings are fused by reciprocal rank; equal fused scores
  follow the keyword order, which sees negation where similarity doesn't.
- A memory of the listed projects counts 30% more in the fused score: it
  wins over a global memory that matches as well, but not over one that
  clearly matches better. Without a topic, project memories come first, then
  the newest.
- Search embeds the question and up to 32 memories that lack a current vector
  (newest first), then compares the question with every current vector in
  one pass, which a server keeps in memory between searches; `reindex`
  embeds the rest, and `embedding set` or
  `embedding sync` re-embeds every memory. Vectors from another model or
  length are never mixed. If the endpoint is unusable, search uses keywords
  only and prints one warning.
- Each result says `"semantic": true` when meaning ranked it beside the
  keywords, and `false` without a topic, without an endpoint, or when the
  endpoint failed. A search with no match and `semantic: false` may have
  missed a memory phrased differently.
- Each result carries `passages`, how many passages the memory has. A
  memory with more than one (a non-empty context counts as one) also
  carries `passage`, the text of its best-matching passage; one found by
  meaning alone, by its title or without a topic shows its first. Read the
  memory for the rest. A memory with one passage has no `passage`: its
  content is that passage.

## The index

- **Rules** are the active explicit, confirmed `preference`, `constraint` and
  `value` memories, grouped by category, largest first. Within a group,
  project memories come first, then those confirmed in more conversations,
  then the newest.
- **Other memories** follow, grouped by type: patterns, goals, then the rest,
  largest first. Stale and superseded memories are listed, marked, only when
  `status` asks for them.
- Each line is `<8-character id> <title>`, marked `[project]` for project
  scope, `(learned in N sessions)` for an observation N conversations
  recorded or confirmed,
  `(tentative)` for an inference or `(stale)`, and `×N` when N
  conversations stated it.
  Every memory id a tool or command takes (`read`, `update`, `forget`, and
  `record`'s `confirms`, `supersedes`, `distinct_from` and `evidence_ids`)
  may be those 8 characters; a prefix several memories share, or none, is
  refused, naming each match.
- `--topic` moves task-relevant memories to the front of their groups.
- When the store cannot be opened or read, the index tool answers
  `Memory unavailable: <reason>`. A call it refuses, such as a local path
  among `projects` or a stale cursor, answers with the refusal alone.

## Pages

`index`, `read --category` and `search` return at most a page: 20 memories
for a search, which ranks, and 100 for the index and a category read, which
list, unless `page_size` (`--page-size`; the tools also take `limit`) asks
for up to 200. A page with more to
come ends with `next_cursor`; pass it as `cursor` (`--cursor`) for the next
page. A cursor names a position in one result's order: after the store
changes, or when mistyped, it is refused with a request to start again, so a
page never skips or repeats a memory. The session hook reads every page, 200
at a time, before shortening the index to its share of the output.

## Conversation links

A session sees the folder's own project. To keep another repository's
memories loaded, as when a session started in a parent folder works across
several repositories, the agent calls `link` with the conversation origin and
the repositories' remotes or project keys:

```json
{"origin": "claude:session/7f3c…", "projects": ["github.com/owner/tools"]}
```

Without an origin, as without the session hooks, `link` links nothing and
says where the origin comes from.

- Links belong to the conversation origin and are stored with the memories,
  so they last through resume and compaction, which keep the session id, and
  reach every machine sharing a server. Another conversation sees none of
  them.
- Linking is idempotent and returns the linked projects with the index lines
  of the ones just added. `projects: []` lists the links; `unlink: true`
  removes the listed ones.
- The folder's own projects and those its `.user-memory` lists always load
  there and are never links: linking or unlinking one changes nothing and
  says where it comes from. The connector names them for a remote server,
  which cannot see the folder.
- Local paths are refused, since they name a project differently on each
  device. Without an origin, as without the session hooks, link is refused;
  pass `projects` to `index`, `search` and `read` instead.
- `forget --origin` removes the conversation's links with its memories.
  Exports leave links out: they are conversation state, not memories.

## Consolidation review

`review` (CLI `user-memory review [--project P]… [--max-groups N]
[--keep ID]…`) runs only when the user asks. It changes no memory. It groups
(each member with its id, title and content):

- **recent** — first: the rules added or changed in the last 30 days, then the
  memories agents recorded (observed or inferred) in that time, by their
  history's time, never their dates, which an import keeps from the file. It
  lists 20 members at most, with `left_out` for the rest. A store imported
  yesterday lists every imported rule: they are new to this store. An
  import that was undone counts for nothing: a memory it gave back is as
  recent as the version it is. A memory whose history was pruned is not
  listed. Show them to the user, and forget a
  rule the user did not give only with their yes. Its members are never
  passed to `keep`, which marks memories that are alike, and the group
  stays listed for 30 days whatever the user answers;
- **stopped** — third, after ended: agents' notes in force that no longer
  hold a command back, because
  agents ran the held command again, unchanged, in three sessions in a row
  ([held back](how-it-works.md#held-back-before-it-runs)); each member's
  `overrides` says in how many. Show them to the user: confirming a note
  makes it hold again, and changing or forgetting it are the other ways;
- **duplicate** — the same memory recorded more than once in one scope
  (merge with `supersedes` + `merge`);
- **redundant** — a project memory that repeats a global one (forget the copy);
- **shared** — the same rule as project memories in several projects with no
  global memory covering them (keep each copy; with the user's yes, record one
  global rule, then forget the copies);
- **overlap** — memories on one topic that may conflict, refine each other, or
  simply share a category (merge them into one rule when a missing condition
  or scope reconciles them and the user agrees; otherwise leave as is);
- **stale** — patterns whose supporting memories changed or were forgotten;
- **ended** — second, after recent: the memories whose
  [condition](memory-model.md#conditions) ended in the last 30 days, each
  with `ended` (when, how, the evidence and the conversation), so the user
  sees every end an agent made and can reopen it with `update` or forget it;
- **ended_support** — patterns citing a memory whose date has passed;
- **open_condition** — tag or text conditions unchanged for over 60 days,
  asking whether each still holds. Nothing records when a condition was
  set, so the memory's `updated_at` stands in for it: any later change to
  the memory restarts the 60 days.

A memory out of force by its condition is compared with nothing: it is in
no duplicate or overlap group.

In an overlap group every member is related to every other, so a chain of
memories each related only to the next is never one group: a memory related
to two that are not related to each other is listed in two groups, and every
related pair appears together in some group. An inferred pattern is never
paired with the memories it cites as evidence. A group lists at most eight
members; a larger group of duplicates or stale patterns lists the closest
and says how many it left out (`left_out`).

Two memories the user calls different can be marked so review stops grouping
them: `user-memory update ID --distinct-from OTHER` (repeatable), or
`update` with `distinct_from` and nothing else. A group, or the members of
one, that the user leaves as they are is kept in one call: `review` with
`keep` (CLI `--keep ID`, repeatable; at least 2, at most 20, the user's yes
via `user_confirmed`) marks every pair among them, reports how many were new
as `kept`, and returns the groups that remain. Marking never changes any
memory. Memories in different category groups are never grouped.
