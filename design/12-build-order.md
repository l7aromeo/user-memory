# 12. Build order

How the open issues depend on each other, the order to build them in, a
proposed v0.11.0, and the risks. The dependencies are the issues'
blocked-by links on GitHub as of 2026-11-05, plus the "Depends on" lines in
their bodies where no link exists.

## 1. Before v0.11: what v0.10.0 must deliver

| v0.10.0 issue | What this design uses |
|---|---|
| #59 | The registry, `<data>/accounts.sqlite3`, with `client_tokens` and `settings`; its own migrations. |
| #48 | `umt_` client tokens, read and write scopes, `host token`, `retire-shared`, the 401 descriptions. |
| #39 | Memory history, which #42 uses for the client. |
| #42 | New rules at session start: #53's option O2. |
| #43 | Import ids and `forget --import`, which `account merge` uses (#67). |

All of v0.10.0 is still open. Nothing below starts before #48 and #59
land; #67 also waits for #43.

## 2. The dependency graph

```mermaid
flowchart LR
    subgraph v010[v0.10.0]
        I48["#48 client tokens"]
        I59["#59 registry"]
        I43["#43 import ids"]
    end
    I57["#57 per-store gates"]
    I58["#58 server key"]
    I69["#69 export source"]
    I60["#60 remotes"]
    I63["#63 accounts mode"]
    I61["#61 switching"]
    I64["#64 GitHub sign-in"]
    I68["#68 connect links"]
    I70["#70 per-user operations"]
    I62["#62 confirm by prompt"]
    I65["#65 GitLab"]
    I66["#66 membership"]
    I67["#67 linked identities"]
    I71["#71 uninstall sign-out"]
    I73["#73 accounts guide"]
    I72["#72 servers playbook"]
    I74["#74 evaluations"]

    I59 --> I48
    I58 --> I60
    I48 --> I60
    I57 --> I63
    I48 --> I63
    I60 --> I61
    I58 --> I64
    I63 --> I64
    I60 --> I68
    I63 --> I70
    I61 --> I62
    I64 --> I65
    I64 --> I66
    I64 --> I67
    I43 --> I67
    I60 --> I71
    I64 --> I71
    I63 --> I73
    I64 --> I73
    I68 --> I73
    I62 --> I72
    I64 --> I72
    I72 --> I74
    I58 -.-> I69
    I67 -.-> I62
    I65 -.-> I66
```

Solid arrows are blocked-by links. Dashed ones are "Depends on" lines with
no link:

- **#69 on #58:** for the fingerprint. Before #58 it uses the store's
  identity hash, so it can land at any time.
- **#62 on #67:** for the account commands' phrases.
- **#66 on #65:** for GitLab membership.

Not in the graph: #53 (a decision) and #54 (research), which block
nothing.

## 3. Phases

Each phase holds the issues whose blockers are all in earlier phases.
Within a phase, the issues can be built in parallel. This matches the
order in #56.

| Phase | Issues | What it builds | Can start when |
|---|---|---|---|
| 1 | #57, #58 | Per-store gates, connections and vector cache; the server key and pin | v0.10.0's #48 and #59 have landed (for #58's endpoint beside them) |
| any | #69 | The export's `source` field | Any time; it uses #58's fingerprint once that lands |
| 2 | #60, #63 | Remotes on the client; accounts mode, per-user stores, allow rules | #58 and #48 (#60); #57 and #48 (#63) |
| 3 | #61, #64, #68, #70 | Switching; GitHub sign-in; connect links; per-user operations | #60 (#61, #68); #58 and #63 (#64); #63 (#70) |
| 4 | #62, #65, #66, #67, #71, #73 | Confirmation by prompt; GitLab; membership; linked identities; uninstall sign-out; the accounts guide | #61 (#62); #64 (#65, #66, #71 with #60); #64 and #43 (#67); #63, #64 and #68 (#73) |
| 5 | #72 | The servers playbook | #62 and #64 |
| 6 | #74 | Evaluations | #72 and #62 |
| any | #53, #54 | The approval decision; Hermes and Pi research | Any time |

#62 can land in phase 4 with the remote phrases and gain the account
phrases when #67 lands, or wait for #67. Its body says "#61, and #67 for
the account commands".

## 4. A proposed v0.11.0

Two ways to cut the release. The user decides.

### Option A: everything in one release

All of #57, #58, #60 to #74, plus #69. One upgrade note, one set of docs.
It is the largest release yet: 19 issues, a new public surface (OAuth
endpoints, `remote`, `login`, `account`, `host accounts|users|allow|provider|invite|key`),
and two registry migrations.

### Option B: switching in v0.11.0, accounts in v0.12.0 (proposed)

**v0.11.0, "one person, several servers":**

| Issue | Why it fits |
|---|---|
| #57 | Needed by accounts later; harmless and measurable now. |
| #58 | The base of every safe switch. |
| #60 | Remotes and the one-time move to `default`. |
| #61 | Switching, with sessions kept on their server. |
| #62 | Confirmation by prompt, with the four remote phrases. |
| #68 | Connect links: on a single-user server, an invite redeems for a client token, with no sign-in. |
| #69 | The export's `source`, with the fingerprint. |
| #53 | The decision, so v0.12 can include O3 if chosen. |
| #54 | The research notes, so a design issue can follow. |

**v0.12.0, "accounts":** #63, #64, #65, #66, #67, #70, #71, #72, #73,
#74, and #62's account phrases.

Why B:

- v0.11.0 then needs no OAuth, no allow rules and no account tables. Its
  only server change is the key and the invite table.
- A single person with a home and a work server gets the whole benefit:
  named remotes, safe switching, pinned keys, invites instead of copied
  tokens.
- The security-heavy parts (brokered OAuth, membership, merges) ship
  together, with their guide, playbook and evaluations.

What B costs:

- #71 depends on #64 for `DELETE /auth/token`, so uninstall's sign-out of
  every remote waits for v0.12.0, unless #48's revoke endpoint covers it
  (open point 2).
- #72 and #74 depend on #64 for their sign-in workflow, so agents get no
  servers playbook in v0.11.0 unless #72 is split (open point 3).

## 5. Risks

| Risk | Where | Mitigation |
|---|---|---|
| The key check is too slow for the prompt hook | #58 | Measure against `PROMPT_LIMIT`; cache per session id, never per process (#58). |
| A client's prompt hook can't tell the user's prompt from automation | #62 | Check each client when building; the docs say where it can't (#62). |
| The registration rewrite to `default` breaks a working install | #60 | It runs once, keeps the token file where it is, and is tested through the binary; environment setups keep their priority. |
| Two registry migrations numbered at once | #63, #68 | Each is "the next version after the one in use"; they are numbered when they land, as #68 says. |
| A code path decides a holder from a path | #63 | A test greps that no code does (#63). |
| Per-store gates slow one store down | #57 | `just search-budget` and `tests/concurrency.rs` on one store, before and after. |
| GitLab device grant support varies by version | #65 | Offer the web way only on an older instance, and say why. |
| GitLab group tokens may need a paid plan on the hosted service | #66 | Confirm before the docs promise it (#66). |
| An agent learns to type the phrase or handle tokens | #62, #72 | #74's evaluations, each with a mutation check. |
| Windows parity | #60, #62, #71 | Owner-only files through `src/private.rs`; the Windows uninstaller does the same as #71 requires. |
| v0.10.0 slips | all | Nothing here starts before #48 and #59; #57 and #58 are the only phase-1 work. |

## 6. Issue map

| Issue | Phase | Option B release | Blocked by (links) | Main file |
|---|---|---|---|---|
| #53 | any | v0.11.0 (decision) | none | [06](06-confirmation-and-approval.md) |
| #54 | any | v0.11.0 (research) | none | [10](10-agent-integrations.md) |
| #57 | 1 | v0.11.0 | none | [04](04-accounts-mode.md) |
| #58 | 1 | v0.11.0 | none | [03](03-identity-and-auth.md) |
| #60 | 2 | v0.11.0 | #58, #48 | [05](05-remotes-and-switching.md) |
| #61 | 3 | v0.11.0 | #60 | [05](05-remotes-and-switching.md) |
| #62 | 4 | v0.11.0 (remote phrases) | #61 | [06](06-confirmation-and-approval.md) |
| #63 | 2 | v0.12.0 | #57, #48 | [04](04-accounts-mode.md) |
| #64 | 3 | v0.12.0 | #58, #63 | [03](03-identity-and-auth.md) |
| #65 | 4 | v0.12.0 | #64 | [03](03-identity-and-auth.md) |
| #66 | 4 | v0.12.0 | #64 | [03](03-identity-and-auth.md) |
| #67 | 4 | v0.12.0 | #64, #43 | [03](03-identity-and-auth.md) |
| #68 | 3 | v0.11.0 | #60 | [05](05-remotes-and-switching.md) |
| #69 | any | v0.11.0 | none | [04](04-accounts-mode.md) |
| #70 | 3 | v0.12.0 | #63 | [04](04-accounts-mode.md) |
| #71 | 4 | v0.12.0 | #60, #64 | [05](05-remotes-and-switching.md) |
| #72 | 5 | v0.12.0 | #62, #64 | [11](11-skill-docs-evals.md) |
| #73 | 4 | v0.12.0 | #64, #68 | [11](11-skill-docs-evals.md) |
| #74 | 6 | v0.12.0 | #72 | [11](11-skill-docs-evals.md) |

## 7. Open points that block scheduling

From this file:

1. **Option A or B.** The user decides how to cut the release.
2. **#71 under option B.** #71 uses `DELETE /auth/token` from #64, and so
   does #60's `remote remove`. If #48 lets a machine revoke its own token
   without accounts, #71 could move to v0.11.0, and #60 needs it anyway.
   #48 needs a line naming the endpoint a machine uses to revoke its own
   token.
3. **#72 under option B.** Splitting #72 into a switching playbook
   (v0.11.0) and a sign-in playbook (v0.12.0) would give agents the
   switching rules when switching ships. That needs an issue edit or a new
   issue.
4. **Missing links.** #68's body depends on #58, #48 and #60, but only #60
   is linked. #73's body depends on #63, #64 and #68, but only #64 and #68
   are linked. #69's dependency on #58 is soft, so it needs no link.
   Proposed links: #68 ← #58 and #73 ← #63, and either #62 ← #67 or a
   sentence in #62 saying the remote phrases ship first.

From the other files:

- [06](06-confirmation-and-approval.md): who revokes a lapsed `login`
  token; the characters allowed in a remote name; #53's decision.
- [07](07-state-machines.md): the damaged-registry message with accounts
  on.
- [08](08-edge-cases.md): the `login` message for a server older than
  accounts.
- [10](10-agent-integrations.md): Hermes hooks and the "app's CLI" rule;
  the name of the list form that replaces `both`.
- [11](11-skill-docs-evals.md): the evaluation harness work #74 assumes.

- [01](01-architecture.md): #61's stop hook doesn't exist; how `serve`
  finds the session's record.
- [02](02-data-model.md): where membership checks are kept; what `purge`
  does to a user's rows; store and user id shapes; allow rules as one row
  each; `client_tokens.user` for the owner; #39's and #42's references
  after #61; the client migration without a reachable server; the
  registry and #52's checks.
- [03](03-identity-and-auth.md): `remote trust` has no issue; messages
  that name a remote before #60; replacing a remote's token after #48's
  revoked or expired messages; the accounts-on notice for pre-accounts
  client tokens.
- [04](04-accounts-mode.md): backups of every store; the registry's quiet
  and shutdown checks; `enable` with an unknown owner; `disable` and
  `purge` without a terminal; removing the owner; a removed allow rule; a
  registry backup naming a purged store; restoring an older backup; gate
  keys by store id; `source.server` before #58.
- [05](05-remotes-and-switching.md): `remote trust`; replacing a remote's
  token; a remote with no token; the `serve` and SessionStart window;
  `remote use` under an environment override; the rules block on a 401;
  uninstall and a key mismatch; the rules header without accounts.
- [09](09-security.md): the token held in `pending-confirmation`; the key
  check's per-session cache.

Shared across files, and the first to settle:

- **`remote trust`** ([03](03-identity-and-auth.md) 1,
  [05](05-remotes-and-switching.md) 1): needed by #58 and #62, owned by no
  issue. Proposed home: #60.
- **The self-revoke endpoint** (open point 2 above): `remote remove` (#60)
  and uninstall (#71) both revoke this machine's token, but the endpoint,
  `DELETE /auth/token`, is defined by #64. #48 should own it, so #60 can
  use it before accounts exist.
- **The client migration without a reachable server**
  ([02](02-data-model.md) 8, [05](05-remotes-and-switching.md) 3): one
  answer in #60.
