# Forget a conversation or a memory

Forget only on the user's explicit request, and say what will be lost before doing it.

## Everything learned in one conversation

1. Identify the conversation's origin: `claude:session/<session id>` for Claude Code, `codex:thread/<thread id>` for Codex. The origin of the current conversation is printed at session start. Ask the user for the id if it is another conversation; never guess it.
2. Call `forget` with `origin` and `user_confirmed: true`.
3. Explain the result:
   - `ids` (`count`): memories deleted, because only that conversation stated them.
   - `kept`: memories that another conversation also stated. They stay, without this conversation as an origin.
   - `unlinked`: how many projects the conversation had linked. They stop loading for it; their memories stay.
   - Patterns that relied on deleted or changed memories go stale and stop appearing in ordinary search. `review` lists them for forgetting or re-deriving.

## A single memory

1. Find it with `search` (add `scope: all` for other projects' memories and `status: all` for stale or superseded ones) and confirm with the user which one, by content and id.
2. Call `forget` with `memory_id` and `user_confirmed: true`.
3. Mention any patterns that cited it; they go stale.

CLI equivalent for the user: `user-memory forget ID` or `user-memory forget --origin ORIGIN`.

Forgetting deletes; an older backup restored later brings the memories back (see [backup.md](backup.md)).
