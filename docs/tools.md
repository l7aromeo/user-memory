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
| `read` | Whole memories by up to 20 ids (full or the index's 8 characters) or by category; unknown ids and ids outside `status` are reported, and `projects` and `scope` given with ids are ignored; a category read takes the [filters](#scope-and-filters) and is paged |
| `search` | Find relevant memories by topic; [filters](#scope-and-filters); paged |
| `record` | Save a typed explicit memory, grounded observation, or evidence-backed inference; if similar memories exist, nothing is stored until the agent confirms, supersedes or marks it distinct |
| `update` | Correct or confirm a memory on the user's request, or with `distinct_from` mark memories the user calls different |
| `forget` | Delete one memory, or every memory and link from one conversation origin, and invalidate dependent inferences |
| `link` | Keep projects loaded for one conversation origin: its session start and change notices include them, after resume and compaction too; returns the linked projects' index lines; `unlink` removes. See [Conversation links](#conversation-links) |
| `review` | On the user's request only: groups of duplicates, cross-scope copies, overlaps and stale patterns; `keep` marks a group the user leaves as is |
| `export` | On the user's request: write every memory, or those the [filters](memory-model.md#exporting-or-importing-part-of-the-store) select, to a JSON file in the exports directory; returns a summary only |
| `import` | On the user's request: preview an export file from the exports directory, or the memories the filters select from it; writes only with `apply` and `user_confirmed` after the user approves |

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
- `status` is `active` (confirmed and tentative) unless set to `stale`,
  `superseded` or `all`. A read by ids takes only `status`: ids outside it are
  listed under `excluded` with their status. An id names one memory in any
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
  common function words are ignored. Each word of the question counts once
  per memory, by the strongest field holding it (category, then title, then
  type, then the text), and by how rare it is among the candidates, so a
  word most memories share adds little. Ties go to the newest. Category
  synonyms match at query time:
  programming/coding/software development/development,
  communication/comms/writing style, and workflow/process.
- **Semantic (optional):** an OpenAI-compatible embeddings endpoint finds
  memories phrased differently from the question. Of the 20 closest, those
  near the best match join every keyword match. "Near" means within three
  quarters of the gap between the best and the median similarity of this
  question's candidates, so a far memory is never listed just to fill the
  count. Both rankings are fused by reciprocal rank.
- A memory of the listed projects counts 30% more in the fused score: it
  wins over a global memory that matches as well, but not over one that
  clearly matches better. Without a topic, project memories come first, then
  the newest.
- Search embeds the question and up to 32 memories that lack a current vector
  (newest first); `reindex` embeds the rest, and `embedding set` or
  `embedding sync` re-embeds every memory. Vectors from another model or
  length are never mixed. If the endpoint is unusable, search uses keywords
  only and prints one warning.
- Each result says `"semantic": true` when meaning ranked it beside the
  keywords, and `false` without a topic, without an endpoint, or when the
  endpoint failed. A search with no match and `semantic: false` may have
  missed a memory phrased differently.

## The index

- **Rules** are the active explicit, confirmed `preference`, `constraint` and
  `value` memories, grouped by category, largest first. Within a group,
  project memories come first, then those confirmed in more conversations,
  then the newest.
- **Other memories** follow, grouped by type: patterns, goals, then the rest,
  largest first. Stale and superseded memories are listed, marked, only when
  `status` asks for them.
- Each line is `<8-character id> <title>`, marked `[project]` for project
  scope, `(tentative)` or `(stale)`, and `×N` when N conversations stated it.
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

- **duplicate** — the same memory recorded more than once in one scope
  (merge with `supersedes` + `merge`);
- **redundant** — a project memory that repeats a global one (forget the copy);
- **shared** — the same rule as project memories in several projects with no
  global memory covering them (keep each copy; with the user's yes, record one
  global rule, then forget the copies);
- **overlap** — memories on one topic that may conflict, refine each other, or
  simply share a category (merge them into one rule when a missing condition
  or scope reconciles them and the user agrees; otherwise leave as is);
- **stale** — patterns whose supporting memories changed or were forgotten.

In an overlap group every member is related to every other, so a chain of
memories each related only to the next is never one group: a memory related
to two that are not related to each other is listed in two groups, and every
related pair appears together in some group. A group lists at most eight
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
