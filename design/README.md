# user-memory v0.11 design: accounts, server switching and the v0.11.0 issues

This folder is the system design for the v0.11.0 milestone: [#53](https://github.com/l7aromeo/user-memory/issues/53), [#56](https://github.com/l7aromeo/user-memory/issues/56),
[#57](https://github.com/l7aromeo/user-memory/issues/57), [#58](https://github.com/l7aromeo/user-memory/issues/58), [#60](https://github.com/l7aromeo/user-memory/issues/60) to [#74](https://github.com/l7aromeo/user-memory/issues/74), [#96](https://github.com/l7aromeo/user-memory/issues/96) and [#97](https://github.com/l7aromeo/user-memory/issues/97) (option A, chosen 2026-11-05). [#54](https://github.com/l7aromeo/user-memory/issues/54) is
covered too, and stays unscheduled. It is the reference for building
v0.11.0 and for the order to build it in.

**Status:** draft, written 2026-11-05 against main 48c5be3 (v0.9.2), and
updated the same day with the user's decisions on its open points:

- v0.11.0 holds every issue here except [#54](https://github.com/l7aromeo/user-memory/issues/54);
- `user-memory remote set-token NAME FILE` replaces a remote's token ([#60](https://github.com/l7aromeo/user-memory/issues/60));
- the window between `serve` and SessionStart is closed: `serve` records
  the remote it resolved, and SessionStart compares ([#61](https://github.com/l7aromeo/user-memory/issues/61));
- rules and sensitive commands use one native confirmation, the terminal
  or a dialog, with the typed phrase only where no display can show one
  ([#53](https://github.com/l7aromeo/user-memory/issues/53), [#62](https://github.com/l7aromeo/user-memory/issues/62); [06](06-confirmation-and-approval.md));
- [#96](https://github.com/l7aromeo/user-memory/issues/96) (the evaluation harness) and [#97](https://github.com/l7aromeo/user-memory/issues/97) (copying a local store into a
  server account) are new.

It assumes v0.10.0 lands first: client tokens in a registry ([#48](https://github.com/l7aromeo/user-memory/issues/48), [#59](https://github.com/l7aromeo/user-memory/issues/59)),
memory history ([#39](https://github.com/l7aromeo/user-memory/issues/39)), the new-rules block at session start ([#42](https://github.com/l7aromeo/user-memory/issues/42)) and
import ids ([#43](https://github.com/l7aromeo/user-memory/issues/43)).

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
| [00-overview.md](00-overview.md) | Goals, non-goals, principles, glossary, scope per issue | [#56](https://github.com/l7aromeo/user-memory/issues/56) |
| [01-architecture.md](01-architecture.md) | Components and deployment shapes | [#56](https://github.com/l7aromeo/user-memory/issues/56) |
| [02-data-model.md](02-data-model.md) | The registry, the stores, the client's files, migrations | [#57](https://github.com/l7aromeo/user-memory/issues/57), [#59](https://github.com/l7aromeo/user-memory/issues/59), [#60](https://github.com/l7aromeo/user-memory/issues/60), [#63](https://github.com/l7aromeo/user-memory/issues/63), [#68](https://github.com/l7aromeo/user-memory/issues/68) |
| [03-identity-and-auth.md](03-identity-and-auth.md) | Server keys, tokens, sign-in, membership, linked identities | [#58](https://github.com/l7aromeo/user-memory/issues/58), [#48](https://github.com/l7aromeo/user-memory/issues/48), [#64](https://github.com/l7aromeo/user-memory/issues/64), [#65](https://github.com/l7aromeo/user-memory/issues/65), [#66](https://github.com/l7aromeo/user-memory/issues/66), [#67](https://github.com/l7aromeo/user-memory/issues/67) |
| [04-accounts-mode.md](04-accounts-mode.md) | Accounts on and off, per-user stores, operations, export source | [#57](https://github.com/l7aromeo/user-memory/issues/57), [#63](https://github.com/l7aromeo/user-memory/issues/63), [#69](https://github.com/l7aromeo/user-memory/issues/69), [#70](https://github.com/l7aromeo/user-memory/issues/70) |
| [05-remotes-and-switching.md](05-remotes-and-switching.md) | Remotes, switching, connect links, uninstall | [#60](https://github.com/l7aromeo/user-memory/issues/60), [#61](https://github.com/l7aromeo/user-memory/issues/61), [#68](https://github.com/l7aromeo/user-memory/issues/68), [#71](https://github.com/l7aromeo/user-memory/issues/71) |
| [06-confirmation-and-approval.md](06-confirmation-and-approval.md) | One native confirmation for sensitive commands and rules | [#62](https://github.com/l7aromeo/user-memory/issues/62), [#53](https://github.com/l7aromeo/user-memory/issues/53) |
| [07-state-machines.md](07-state-machines.md) | A remote, an account, a client token, a server's accounts mode | all |
| [08-edge-cases.md](08-edge-cases.md) | One matrix of scenarios and their outcomes | all |
| [09-security.md](09-security.md) | Threat model | all |
| [10-agent-integrations.md](10-agent-integrations.md) | Hermes Agent and Pi | [#54](https://github.com/l7aromeo/user-memory/issues/54) |
| [11-skill-docs-evals.md](11-skill-docs-evals.md) | The servers playbook, the accounts guide, the evaluations, copying a local store | [#72](https://github.com/l7aromeo/user-memory/issues/72), [#73](https://github.com/l7aromeo/user-memory/issues/73), [#74](https://github.com/l7aromeo/user-memory/issues/74), [#96](https://github.com/l7aromeo/user-memory/issues/96), [#97](https://github.com/l7aromeo/user-memory/issues/97) |
| [12-build-order.md](12-build-order.md) | Phases, the issue map, what v0.11.0 holds, risks | all |

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
lists what is still open.
