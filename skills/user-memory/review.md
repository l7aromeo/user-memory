# Review and consolidate memories

1. Call `review`. Inside a project, pass `projects` with the projects the session start's `Projects:` line lists, the folder's own, the listed and the linked ones (or the output of `user-memory project-id`), so their memories are reviewed with the global ones; list more projects to review them too. Raise `max_groups` (up to 20) if the user wants more groups.
2. Present each group by kind, with each member's id, type, scope and a short paraphrase of its content:
   - `duplicate`: the same memory recorded more than once in one scope.
   - `redundant`: a project memory that repeats a global one.
   - `shared`: the same rule as project memories in several projects, with no global memory covering them. Each project still needs its copy.
   - `overlap`: memories on one topic, each related to every other in the group. They may overlap, one may be an exception to the other, or they may simply be unrelated memories in the same category. A memory related to two that are not related to each other appears in two groups.
   - `stale`: patterns whose supporting memories changed or were forgotten.

   A group that lists only some of its members gives the number left out as `left_out`: say so, and review again once its listed members are handled.
3. Propose one action per group and let the user choose:
   - Duplicate: merge into one memory, or forget the extras.
   - Redundant: forget the project copy; the global memory already applies there.
   - Shared: leave the copies, unless the user wants the rule everywhere. Then, with their yes, record it once with scope global and `user_confirmed: true`, and only after that forget the project copies. Never forget a copy on its own: each project would lose the rule.
   - Overlap: memories that seem to conflict usually combine into one rule once a missing condition or scope is found (the user's own repositories versus others', one project versus all). A rule newer memories do not mention still holds; only the user drops a rule. When the merged rule is clear, propose it and merge after the user's yes; when in doubt, ask. Never forget one side silently. Otherwise leave the group as is.
   - Stale: forget it, or re-derive it from current evidence.
4. Carry out only the approved actions:
   - Merge: call `record` with the consolidated `content` and a one-line `title` that states it, the members' `type`, `category`, `scope` and `project`, `supersedes` listing every member id, and `merge: true`. If any member is explicit, use `source: explicit`. Pass `user_confirmed: true` when any member is explicit or the scope is global: the user approved this merge. Pass the conversation `origin` printed at session start. The merged memory keeps every conversation its members came from.
   - Forget: call `forget` with `memory_id` and `user_confirmed: true`, one call per memory.
   - Re-derive: call `record` with a `title`, `source: inferred`, `evidence_ids` naming at least two current, independent supporting memories, and `supersedes` naming the stale pattern. A global pattern also needs `user_confirmed: true`, once the user approved it. It stays tentative.
   - Change wording: call `update` with `memory_id`, the changed fields and `user_confirmed: true`. The memory keeps its evidence and reason unless the call gives `evidence` or `reason`. A `title` given alone only rewords the index line: the memory keeps its conversations and status. Rewording an observation or inference (title, context, reason or evidence) keeps it the agent's; changing its content, type, category, scope or project makes it the user's explicit statement.
5. If the user says two grouped memories are different, mark them distinct after they say so: call `update` with the `memory_id` of one, `distinct_from` listing the others the user named, and `user_confirmed: true`. Give no other fields; the memories themselves do not change. Review no longer links those pairs.
6. Report what was merged, forgotten or re-derived, with the new and old ids. Patterns that cited a changed or superseded memory go stale; mention any that the next review will list.

CLI equivalent for the user: `user-memory review [--project PROJECT]… [--max-groups N]`, then `user-memory record … --supersedes ID --supersedes ID --merge`, `user-memory forget ID` or `user-memory update ID --distinct-from OTHER`.
