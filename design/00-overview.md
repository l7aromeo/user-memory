# 00. Overview

## What this design is for

user-memory is each person's own memory, shared across their agents,
clients and machines. Today a server is one person's memory behind one
token. The open issues turn that into:

- a server that can hold several people, each fully isolated (#56, #63);
- accounts that are optional: an install that never turns them on sees no
  change (#56);
- sign-in with GitHub or GitLab, brokered by the server, so a provider's
  token never reaches a client (#64, #65, #66, #67);
- a machine that knows several servers by name and switches between them
  safely, at the command line or with the agent's help (#58, #60, #61, #68,
  #71);
- an agent that never handles a token and never switches, signs in or
  changes accounts without the user (#62, #72, #74);
- an honest answer to "can the server tell the user really approved a
  rule?" (#53);
- room for more agent apps than Claude Code and Codex (#54).

## Goals

1. **Isolation.** One user's request can only open that user's store. No
   code path reads two users' memories in one call.
2. **Nothing changes for a single user.** A native install, or a server
   with one token, keeps the same files, schema, registrations, messages
   and doctor lines, apart from doctor's server line naming the server
   (#56). A test checks that no new file appears.
3. **No data loss on any transition.** Turning accounts on or off,
   removing a user, switching servers, merging accounts and restoring a
   backup never delete memories. Only `host users purge` deletes, and only
   after it offers an export (#63).
4. **The agent can't act for the user.** Every command that changes where
   memory goes or whose it is needs the user at a terminal, or the
   command's exact phrase as the user's whole next prompt (#62). There is
   no `--yes`.
5. **A token goes only to the server it belongs to.** Every process checks
   the server's key before it sends one (#58).
6. **Every edge has a defined outcome and message.** [08-edge-cases.md](08-edge-cases.md)
   lists them.

## Non-goals

These are out of scope for v0.11, with the reason. Most are listed as
"Later" in #56.

| Not in v0.11 | Why |
|---|---|
| Groups, group stores and group rules | The design leaves room (a store's holder kind, #63), but no issue builds them. |
| Converting a shared store into a group's store | Depends on groups. |
| The OS keychain for client tokens | Owner-only files are the same protection the store has; a keychain is per-platform code. |
| An `/account` web page | The CLI covers revoking a lost machine (`account clients revoke`, #67). |
| An actor field in memory history | #39 records the client; the user is implied by the store until groups exist. |
| Self-service restore | Restore replaces a store while it is served; it stays the operator's (#70). Users export at any time. |
| Proving a person approved a rule | No scheme stops a program running as the user from forging a yes on purpose ([06](06-confirmation-and-approval.md), #53). |
| Isolation from the server's operator | The operator can read every store. The accounts guide says so (#73). |

## Principles

- **The issues are the contract.** Commands, flags, files and messages
  here are the issues' own. Where this design proposes a change, the file
  says so under "Open points".
- **Server-wide things stay server-wide.** The embedding setting, the
  schema version (one `migrate` upgrades every store) and the registry are
  per server, not per user (#56).
- **Per-store things stay per store.** Backups, write gates, held
  connections and the vector cache are per store (#57, #63, #70).
- **Nothing decides a holder from a path.** doctor, backup, restore,
  repair and migrate read who holds a store from the registry (#63).
- **Running sessions keep their server.** A switch takes effect for new
  sessions; a running one says so once (#61).
- **Additive contract changes only.** New commands, files, fields and
  messages. The environment variables keep their meaning and priority
  (#60). Export gets a `source` field without a format version bump (#69).

## Glossary

| Term | Meaning |
|---|---|
| store | One SQLite memory file, at the current store schema. One per user on a server with accounts. |
| registry | The server's `<data>/accounts.sqlite3`, outside every store. #59 creates it for client tokens; #63 and #68 add their tables by migration. |
| operator | Whoever runs the server and its `host` commands. |
| owner | The user named by `host accounts enable --owner`. The existing store and the shared token become theirs. |
| user | A person with a store on a server. Opaque id, display login. |
| identity | A provider instance plus the provider's numeric user id, written `github:alice` or `gitlab.example.com:42`. Several identities can belong to one user. |
| provider | A GitHub App or a GitLab OAuth application the operator adds (`host provider add`). |
| client token | A `umt_` token issued to one machine for one remote (or to one agent by the operator), read or write, expiring after 90 days unused (#48). |
| shared token | Today's single token file. It keeps working as the owner's until `host token retire-shared`. |
| remote | A named server on a client machine, in `~/.config/user-memory/remotes.json` (#60). `local` names this machine's store. |
| active remote | The remote new sessions use. Set by `remote use`. |
| session's server | The remote, key and user a session recorded at its start. Its MCP process and hooks keep using it (#61). |
| pin, fingerprint | The server key a client trusts for a remote, shown as `SHA256:<base64>` (#58). |
| connect link | An invite URL that carries the server's fingerprint after `#` (#68). |
| accounts mode | Whether a server routes tokens to per-user stores (`host accounts enable`) or serves one store. |
| dormant | A user whose store is kept but not served while accounts are off (#63). |
| removed | A disabled user whose store waits in `removed/` for restore or purge (#63). |
| confirmation phrase | The exact text a user types as their next prompt to apply a pending command (#62). |

## Scope per issue

| Issue | Title (short) | Main file | Also in |
|---|---|---|---|
| #53 | Approval the server can rely on for rules | [06](06-confirmation-and-approval.md) | [09](09-security.md) |
| #54 | More agent apps: Hermes Agent and Pi | [10](10-agent-integrations.md) | [01](01-architecture.md) |
| #56 | Accounts, isolation and switching (tracking) | this file | all |
| #57 | Per-store write gate, connection and vector cache | [04](04-accounts-mode.md) | [01](01-architecture.md), [02](02-data-model.md) |
| #58 | Server key and client pin | [03](03-identity-and-auth.md) | [05](05-remotes-and-switching.md), [07](07-state-machines.md) |
| #60 | Remotes | [05](05-remotes-and-switching.md) | [02](02-data-model.md), [07](07-state-machines.md) |
| #61 | Switching remotes | [05](05-remotes-and-switching.md) | [07](07-state-machines.md) |
| #62 | Confirming by the next prompt | [06](06-confirmation-and-approval.md) | [05](05-remotes-and-switching.md) |
| #63 | Accounts on and off, per-user stores, allow rules | [04](04-accounts-mode.md) | [02](02-data-model.md), [07](07-state-machines.md) |
| #64 | Sign-in with GitHub | [03](03-identity-and-auth.md) | [07](07-state-machines.md) |
| #65 | Sign-in with GitLab | [03](03-identity-and-auth.md) | |
| #66 | Membership re-checks | [03](03-identity-and-auth.md) | [07](07-state-machines.md) |
| #67 | Linked identities, merge, account commands | [03](03-identity-and-auth.md) | [06](06-confirmation-and-approval.md) |
| #68 | Connect links | [05](05-remotes-and-switching.md) | [02](02-data-model.md) |
| #69 | Export source | [04](04-accounts-mode.md) | [02](02-data-model.md) |
| #70 | Per-user restore, repair, migrate, sizes | [04](04-accounts-mode.md) | |
| #71 | Sign out of every remote on uninstall | [05](05-remotes-and-switching.md) | |
| #72 | The skill's servers playbook | [11](11-skill-docs-evals.md) | |
| #73 | The accounts guide | [11](11-skill-docs-evals.md) | |
| #74 | Evaluations of switching, sign-in and moving | [11](11-skill-docs-evals.md) | |

v0.10.0 issues this design builds on: #48 and #59 (tokens and the
registry), #39 (history), #42 (new rules at session start), #43 (import
ids), #87 (the keyword index's stored content).

## Open points

None for this file.
