# Forget a conversation or a memory

Forget only on the user's explicit request, and say what will be lost before doing it.

## Everything learned in one conversation

1. Identify the conversation's origin: `claude:session/<session id>` for Claude Code, `codex:thread/<thread id>` for Codex. The origin of the current conversation is printed at session start. Ask the user for the id if it is another conversation; never guess it.
2. Call `forget` with `origin` and `user_confirmed: true`.
3. Explain the result:
   - `ids` (`count`): memories deleted, because only that conversation stated them.
   - `kept`: memories that another conversation also stated. They stay, without this conversation as an origin.
   - `unlinked`: how many projects the conversation had linked. They stop loading for it; their memories stay.
   - `staled`: patterns that relied on deleted or changed memories and went stale. They stop appearing in ordinary search; `review` lists them for forgetting or re-deriving.
   - `left_superseded`: memories a deleted memory had replaced. They stay hidden, with no active memory in their place. If the user wants one back, `update` it with `user_confirmed: true` to make it active again.
   - `next`: present when either list has entries, with the next step for each, one line each.
   - `backup`: when memories were deleted, the copy of the store made just before, kept for a week beside the automatic backups.

## What one import brought

An applied import that added or replaced memories gives its name in its summary, as `import`, such as `shared.json@2026-11-15T10:42:07Z.e0e0e0e0`.

1. Get the name from the import's summary or the user; never guess it. When there were later imports too, undo the latest first.
2. Call `forget` with `import` set to that name and `user_confirmed: true`.
3. Explain the result:
   - `ids` (`count`): memories the import added, now deleted.
   - `restored`: memories the import had replaced, given back what they were.
   - `kept` and `kept_since`: memories changed since the import, which stay as they are, each with what changed it: a confirmation, correction, supersede, merge, pair marked as different, project move or revive, by its operation's name; `import` for a later import, which `next` says to undo first when it can be; `origin` for a conversation that stated it since; `support gone` for a pattern whose earlier support was forgotten.
   - `backup`, `staled`, `left_superseded` and `next` as for a conversation; `next` names a later import to undo first, or an earlier one to undo again.
   - `reason`: the import was undone already, or no memory carries that name.

## A single memory

1. Find it with `search` (add `scope: all` for other projects' memories and `status: all` for stale or superseded ones) and confirm with the user which one, by content and id.
2. Call `forget` with `memory_id` and `user_confirmed: true`.
3. Name what the result lists: `staled` (patterns that cited it and went stale) and `left_superseded` (memories it had replaced, now with no active memory in their place). Ask whether the user wants any of the latter back; `update` with `user_confirmed: true` makes it active again.

CLI equivalent for the user: `user-memory forget ID`, `user-memory forget --origin ORIGIN` or `user-memory forget --import "NAME"`.

A forgotten memory's history keeps only tombstones, each change's operation and time; `read` with `history: true` shows them under `forgotten`. A correction is not a forget: the memory's history keeps the text it replaced for a year. When the user wants corrected text gone because it is sensitive, `update` with `drop_history: true` and `user_confirmed: true` reduces the memory's earlier history to tombstones as well.

Forgetting deletes; an older backup restored later brings the memories back (see [backup.md](backup.md)).

When the user calls what was forgotten sensitive, such as a secret or a client's name, say once that copies made before the forget still hold it: the automatic backups, the upgrade, repair and project-move backups beside the store, any export, the title in session files for up to 30 days, other machines' instruction files until their next session start, and a non-local embeddings endpoint that embedded it. Text a correction replaced also stays in the memory's history until `drop_history` or the history's yearly pruning removes it. To remove every copy, the user runs `user-memory backup now` where the store lives, then deletes the older backups that `user-memory backup list` shows, the upgrade, repair and project-move backups, and any export from before the forget. Leave the deletions to the user: each one removes a way back.
