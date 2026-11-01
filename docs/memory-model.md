# Memory model

What a memory holds, the rules the server enforces when one is recorded, how
duplicates are resolved, and what an export file holds. For the ideas in
plain words, see [how it works](how-it-works.md).

- [Fields and limits](#fields-and-limits)
- [Provenance rules](#provenance-rules)
- [Duplicates and corrections](#duplicates-and-corrections)
- [Export and import](#export-and-import)

## Fields and limits

| Field | Meaning |
|---|---|
| `type` | Extensible label: preference, constraint, value, decision, fact, goal, behavior, pattern, or another type; a rule is a preference, constraint or value |
| `category` | Topic independent of type: programming, communication, workflow, learning, etc. |
| `title` | One line of at most 120 characters stating the memory so an agent can act on it, such as "Push, publish or tag only with the user's go-ahead"; the index lists it |
| `content` | Concise statement, choice, observation, or tentative pattern |
| `context`, `reason` | When it applies and its stated/evidence-based rationale; absent reasons stay blank |
| `scope`, `project` | Global across projects, or limited to one project key (see [project identity](projects.md#project-identity)) |
| `source` | Explicit user statement, observed action, or inferred pattern |
| `evidence`, `evidence_ids` | Concrete observation note, or the user's own words for an explicit memory; only inferred patterns cite supporting memory IDs |
| `origins`, `confirmations` | Conversations that stated it, such as `claude:session/<id>` (empty when unknown), and how many; `last_confirmed_at` records the latest restatement |
| `status` | Confirmed, tentative, stale after supporting evidence changes, or superseded after the user replaced it |
| `superseded_by` | ID of the replacing memory when superseded, otherwise null |
| `id`, timestamps | Stable UUID v4 identity and creation/update time |

`content` holds at most 4,000 characters and `context`, `reason` and
`evidence` at most 2,000 each, after runs of whitespace collapse; a longer
one is refused with the field, its length and the limit, never cut, and the
same limits apply to imported files. A rule with its conditions and
exceptions fits in one memory: the index and the instruction files show only
titles, so long content is read only when an agent opens the memory. The
same holds for `title` (120), `type` (40), `category` (80) and the project
key (240). Rewording only the `title`
(`update ID --title …`) keeps the memory's conversations, source and status
(only `updated_at` changes, as with any edit), and stales no pattern built on
it.

## Provenance rules

The server enforces these when a memory is recorded or changed:

- **Explicit** statements require the agent to truthfully set
  `user_confirmed=true`.
- **Global** memories apply in every project, so each one, whatever its
  source, also requires `user_confirmed=true`: the agent asks the user first
  or records it with scope project. `scope` has no default. A global memory
  typed on the CLI counts as the user's yes.
- **Observed** behavior requires a concrete evidence note and stays tentative.
- **Inferred** patterns cite at least two independent active explicit/observed
  memories and stay tentative. Independence counts distinct conversations
  across all supporting memories; each memory without an origin counts as its
  own, so recording origins makes the check meaningful. Project evidence cannot
  support a broader global inference. Forgetting a conversation stales a
  pattern that no longer has two independent conversations behind it.
- A pattern also goes stale when a supporting memory is forgotten, superseded
  or made stale, changes what it says or where it applies (type, category,
  context, content, scope, project, evidence note), or is weakened (explicit to
  observed, confirmed to tentative). Confirming a support (observed to
  explicit, tentative to confirmed) and rewording its reason or title only
  strengthen or reword it, so they stale nothing.

These checks validate structure and provenance claims; they cannot prove that
the agent witnessed an event or that an inference is accurate. Memory is
fallible context: current instructions and project constraints take
precedence.

Example explicit preference (`record` arguments):

```json
{
  "type": "preference",
  "category": "programming languages",
  "title": "Prefer Rust over Python",
  "content": "Generally prefer Rust over Python",
  "reason": "Performance and strict safety",
  "scope": "global",
  "source": "explicit",
  "origin": "claude:session/7f3c…",
  "user_confirmed": true
}
```

## Duplicates and corrections

Before a memory is stored, the server looks for active memories that may say
the same thing where the new one applies (global memories, plus the project's
for a project memory): the same category group with heavily overlapping content
words, or — with embeddings configured — a close document embedding under a
cutoff calibrated for the model. The server never decides that two memories
mean the same. When any are found nothing is stored; the reply lists up to
three of them and the agent records again with exactly one of:

- `confirms: <id>` — the user said it again: the existing explicit memory gains
  this conversation (`origins`, `confirmations`, `last_confirmed_at`) instead
  of a copy;
- `supersedes: [<id>…]` — the user changed their mind: the old memories become
  superseded and the new one keeps only this conversation. With `merge: true`
  and two or more memories it consolidates duplicates the user approved
  merging, keeping all their conversations;
- `distinct_from: [<id>…]` — genuinely different: every reported memory must be
  listed; the pair is remembered so review does not group it again.

Details:

- An exact repeat from a conversation that already stated a memory (or from no
  conversation, when the memory has none) is a harmless no-op, so retries are
  safe.
- Observations from different conversations are never duplicates: they are
  independent evidence.
- A narrower project exception next to a general preference ("use Go in this
  repository" next to "prefer Rust") is usually recorded directly; similarity
  cannot tell it from related memories, and review lists such pairs as
  overlaps. An exception that restates one setting with another value ("wrap
  at 100" next to "this repository wraps at 80") can be flagged like a change
  of mind; the agent then records it with `distinct_from`.
- A memory supersedes only one with the same scope and project, so a project
  exception never retires a global preference. Only an explicit statement can
  supersede an explicit memory; superseded memories stay stored, hidden from
  ordinary search and the index, and cannot be updated while the memory that
  replaced them exists. Once that one is forgotten, `update` with the user's
  yes makes such a memory active again (`revived: true`).
- `update` is the in-place correction for a mistake in what was
  recorded. It keeps the memory's conversations: `origin` adds the one the
  correction came from, and `replace_origins` with it leaves only that one,
  for a full restatement in a new conversation. A content change still stales
  dependent inferences. It keeps the evidence too, often the user's own
  words, and the user's stated reason: `evidence` or `reason` replaces it,
  and an empty one clears it. Rewording only (title, context, reason or
  evidence) keeps an observation or inference the agent's: its source,
  status and supporting memories stay. Changing what a memory says or where
  it applies (content, type, category, scope or project), or an update with
  none of these fields (the user confirming it), makes it the user's
  confirmed explicit statement: an inference drops the supporting memories
  it cited (only inferences cite them), and an observation or inference
  drops the agent's explanation in `reason`, which the user never stated.
- Embedding happens before the store is locked, so a cold local model (up to
  about six seconds on first use) delays only that record, never other
  writers. `doctor` says when the configured model has no calibrated cutoff
  and duplicate detection is keyword only.
- Cutoffs: how close counts as a duplicate depends on the embedding model;
  [embedding settings](configuration.md#embedding-settings) says how each
  model's cutoff is set.
- A remote client refuses an older memory server that cannot resolve similar
  memories, instead of silently storing duplicates.

CLI: `user-memory record … --confirms ID`, `--supersedes ID` (repeatable, add
`--merge` to consolidate), `--distinct-from ID` (repeatable).

## Export and import

- **Export** writes the whole store — active, stale and superseded memories,
  their conversations and the pairs marked as different — as one readable
  JSON document. Vectors and the search index are derived and rebuilt with
  `reindex`.
- **Import** always shows a preview first: what would be added or left
  unchanged, conversations merged into identical memories, conflicts by field
  name, invalid rows, and inferred patterns that would go stale. Nothing is
  written without `--apply`. To predict exactly which patterns go stale, a
  `use-file` preview tries its replacements in a transaction and rolls them
  back: it changes nothing stored, but holds the store's write lock meanwhile,
  so other writers wait, and a large one takes about as long as applying.
  Applying from the CLI counts as your confirmation of every explicit memory
  in the file. Conflicts abort by default;
  `keep-store` keeps the store's version and `use-file` takes the file's.
- Exports are format version 2 and carry each memory's title. Version 1
  files, written before titles existed, import only with a
  [titles map](storage.md#titles-for-older-memories) (`--titles FILE`, or the import
  tool's `titles`) for each memory the store does not hold; memories it holds
  keep their titles, and map entries for them are reported as not used.
  Without the map, import changes nothing and lists the memories that need a
  title (the tool by id only). A field over its limit is refused, not cut.
- Restoring an older backup brings back memories forgotten since.
- A client connected to a remote server exports from and imports into that
  server, as with a local store: the file is written on the client, mode 0600,
  and the server's refusals come through. It needs no permission beyond the
  access token, which already reads and writes every memory. The server's
  `/export` and `/import` endpoints take the same token and host checks as
  `/mcp`, and refuse an import request over 128 MiB (a 64 MiB file and a
  titles map) before reading it; the file itself is held to 64 MiB.
- Agents export and import only by file name inside the exports directory beside
  the store (`~/.local/share/user-memory/exports/`,
  `%LOCALAPPDATA%\user-memory\exports\` on Windows, or `/data/exports/` in
  Docker), and only see summaries, never the file's memories.

### Exporting or importing part of the store

Export and import take the [filters](tools.md#scope-and-filters) `read` takes:
`--scope`, `--project` (repeatable; on the CLI also a folder such as `.`,
resolved to its project on this machine), `--category` (with its synonyms),
`--type`, `--source`, `--status` and `--origin`; the tools take the same
fields, with project keys only. Without any, they cover every memory. With some, they mean what they
mean for `read`, except that an unset `--status` is every status and an unset
`--scope` with no `--project` is every scope.

- A filtered export records its filters in the file (`selection`), and the
  import summary shows them as `file_selection`.
- A filtered import imports only the file's memories the filters select. Its
  summary gives `selected` and `skipped` (`not_selected`,
  `unsupported_inferences`); added, unchanged and conflicts count the
  selected memories only. A filter that selects nothing writes nothing and
  says so.
- A filter selects memories and never edits one, so links can point outside
  the selection. The export summary counts them (`links_outside`: supports,
  replacements, distinct pairs). Import settles each by what is there, among
  the imported memories or in the store:
  - a pair marked as different with an end missing is dropped
    (`dropped_distinct_pairs`);
  - a superseded memory whose replacement is missing keeps its link
    (`replacements_missing`), as the store keeps one whose replacement was
    forgotten;
  - an active pattern with a missing support is skipped
    (`unsupported_inferences`): importing it would claim evidence the store
    does not hold. Import the supports first, or with it, to keep it. In an
    unfiltered file a missing support is refused, since only a filter can
    leave one out. Stale and superseded patterns keep their supports' ids.

```bash
user-memory export backup.json            # atomic, mode 0600; --force replaces
user-memory export - > backup.json        # stdout; the summary goes to stderr
user-memory export work.json --scope project --project github.com/me/app
user-memory export rules.json --type preference --status active
user-memory import backup.json            # preview: writes nothing
user-memory import backup.json --apply [--on-conflict abort|keep-store|use-file]
user-memory import backup.json --apply --category programming
user-memory import - --apply < backup.json
user-memory import old-backup.json --titles titles.json   # format version 1
```

An applied import's summary ends with `next` when a step is worth taking:
`review` when `similar_to_existing` is above zero, and an optional `reindex`
when memories were added, since search embeds missing ones as it goes,
up to 32 at a time. A client of a server names where reindex runs, which it
refuses itself. A preview has no `next`. Asked for an export name that is
not there, the import tool lists the newest names in the exports directory,
which an agent cannot list itself.
