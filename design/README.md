# user-memory v0.11 design: accounts, server switching and the open issues

This folder is the system design for every open issue that has no
milestone yet: #53, #54, #56, #57, #58 and #60 to #74. It is the reference
for deciding what v0.11.0 holds and in what order it is built.

**Status:** draft for review, written 2026-11-05 against main 48c5be3
(v0.9.2). It assumes v0.10.0 lands first: client tokens in a registry
(#48, #59), memory history (#39), the new-rules block at session start
(#42) and import ids (#43).

**Where it lives:** the draft branch `docs/design-v0.11`. That branch is
never merged. It is deleted once the work is implemented, and the code, the
docs and the issues then describe the system. The issues stay the source
of truth until then: where a file here differs from an issue, it says so
under "Open points" and names the edit the issue needs.

## How to read it

Start with [00-overview.md](00-overview.md) for the goals, the vocabulary
and which file covers which issue. Then read in the order of the table, or
jump to the part you are deciding.

| File | Covers | Issues |
|---|---|---|
| [00-overview.md](00-overview.md) | Goals, non-goals, principles, glossary, scope per issue | #56 |
| [01-architecture.md](01-architecture.md) | Components and deployment shapes | #56 |
| [02-data-model.md](02-data-model.md) | The registry, the stores, the client's files, migrations | #57, #59, #60, #63, #68 |
| [03-identity-and-auth.md](03-identity-and-auth.md) | Server keys, tokens, sign-in, membership, linked identities | #58, #48, #64, #65, #66, #67 |
| [04-accounts-mode.md](04-accounts-mode.md) | Accounts on and off, per-user stores, operations, export source | #57, #63, #69, #70 |
| [05-remotes-and-switching.md](05-remotes-and-switching.md) | Remotes, switching, connect links, uninstall | #60, #61, #68, #71 |
| [06-confirmation-and-approval.md](06-confirmation-and-approval.md) | Confirming by the next prompt, approval for rules | #62, #53 |
| [07-state-machines.md](07-state-machines.md) | A remote, an account, a client token, a server's accounts mode | all |
| [08-edge-cases.md](08-edge-cases.md) | One matrix of scenarios and their outcomes | all |
| [09-security.md](09-security.md) | Threat model | all |
| [10-agent-integrations.md](10-agent-integrations.md) | Hermes Agent and Pi | #54 |
| [11-skill-docs-evals.md](11-skill-docs-evals.md) | The servers playbook, the accounts guide, the evaluations | #72, #73, #74 |
| [12-build-order.md](12-build-order.md) | Phases, the issue map, a proposed v0.11.0, risks | all |

## Conventions

- Commands, flags, file names and user-visible messages are quoted exactly
  as the issues define them. A message shown in an `output` block is text
  the code prints.
- Each sequence and state diagram is followed by its error paths in text.
- Examples are synthetic: alice and bob, `memory.example.com`,
  `memory.home.example`, `github:alice`, `gitlab.example.com:42`.
- "The operator" runs a server. "The user" is a person whose memories it
  holds. On a native install, one person is both.
- "Today" means main 48c5be3 plus v0.10.0.

## Open points

Every file ends with its own "Open points". [12-build-order.md](12-build-order.md)
gathers the ones that block scheduling.
