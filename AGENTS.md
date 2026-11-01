# AGENTS.md

The rules a change to user-memory is held to, for anyone making one, by hand
or with a tool. [CONTRIBUTING.md](CONTRIBUTING.md) covers setting up and
checking a change, [development](docs/dev/development.md) the tests and
tools, and [releasing](docs/dev/releasing.md) the release. This file links
to them rather than repeating them. Propose changes to it in an issue.

## Instructions and authorship

- AGENTS.md is the only instruction file in the repository. A tool that
  reads another name, such as `CLAUDE.md`, gets a local link to this file,
  never committed.
- Using AI tools is fine. The person who submits a change is its author and
  answers for it as for code written by hand. Commits, pull requests and
  code carry no tool attribution (no `Co-Authored-By` lines for tools) and
  no notes on how the work was organised: only what a reviewer needs to
  understand the change.

## What users rely on

- The CLI, the MCP tools, the files on disk and the store format are
  published. Their observable output is the contract: JSON fields, messages,
  exit codes and defaults. New information goes alongside, as a new field
  or option.
- A breaking change waits for a planned release with an upgrade note. A
  renamed command, flag, tool or field keeps the old name as a deprecated
  alias for at least one minor release.
- Public names are decided once. Propose a rename or a new shape in an issue
  before building it.
- The store holds real memories. A change to its schema (tables, triggers,
  search-index SQL) bumps `SCHEMA_VERSION`, adds a schema snapshot and ships
  a tested forward migration from the previous version, which setup runs
  after an automatic backup. Never drop or rewrite stored memories to make a
  change simpler.
- Your own memories are real data too. A build you run by hand gets its own
  store and home: `USER_MEMORY_DB`, `HOME` and the XDG folders, as
  [development](docs/dev/development.md#run-a-checkout-build-against-a-throwaway-store)
  shows. Without them it opens your real store, and `session-start` and
  `prompt-submit` write into your state folder.
- Measure on real data only on a private copy of the store, never the live
  one, and delete the copy afterwards
  ([measure on a copy](docs/dev/development.md#measure-on-a-copy-of-your-store)).
  Reading the live store changes it too: with semantic search on, a search
  saves vectors for memories that lack one.
- Fixtures, benchmarks, test data and reports hold synthetic memories, ids
  and numbers, never real memories or their wording.

## Code

- Fix the cause, not the symptom: change the order, the data structure or
  the contract that lets the problem happen. A cap, retry or special case
  may sit beside that fix, never instead of it. A known defect is fixed or
  written up in an issue, never left quietly. A new feature handles its edge
  cases and errors fully.
- Design from scenarios. Write the concrete cases, check what already
  exists and how comparable tools do it. Do what every user would want
  without asking. Add a switch only when both outcomes are valid choices,
  and say why a default is the default.
- Performance and features come together, through technique: index or
  precompute once, batch across expensive boundaries, do work lazily. A
  performance change comes with a measurement and a budget.
- Before writing a helper, check whether a dependency or the crate already
  has it.
- No magic values: a limit, timeout, size or format value is a named
  constant beside what it governs, with a doc comment that says why it has
  that value. 0, 1 and indices stay literal.
- Comments say what the code is and why, never its history; that belongs
  in the commit message. One line when one line will do.

### Rust

- Edition 2024 and current idioms: `foo.rs` beside `foo/`, never `mod.rs`;
  let-else and let-chains. Move to a dependency's newer API rather than
  pinning an old version.
- Panic only on a broken invariant. Expected failures (I/O, bad input, a
  missing file) are a `Result` returned with `?`. No `unwrap` outside tests
  (clippy denies it); an `expect` names the invariant that makes it
  unreachable.
- `unsafe` code lives only in `src/platform/windows.rs`, each block with a
  `// SAFETY:` comment; the crate denies `unsafe_code` everywhere else.
  Private files go through `src/private.rs` and per-OS locations through
  `src/paths.rs`, never mode bits or `HOME` at the call site.
- Many linked or same-lifetime objects go in an arena with typed handles,
  not one `Box` or `Rc` each. Integers written as text in a hot loop use
  `format_into` with a reused `NumBuffer`, not `write!` or `to_string`.

## Tests and evidence

- New behaviour comes with a test, and a fix with a test that fails without
  it. Show it failing once before trusting it: a check that cannot fail is
  no check.
- One case per distinct way a change can fail. Extend an existing case
  rather than adding one that catches nothing new. Keep a separate case for
  a past regression, another platform or code path, an ordering hazard, a
  user-facing message or a boundary value.
- Test real scenarios through the real binary, failing on `main` and
  passing on the branch. List the edge cases, and check the old behaviour
  rather than assuming it.
- A test that checks a list is complete ("every tool is documented") gets a
  mutation check: remove one entry and see it fail. See
  [mutation testing](docs/dev/development.md#mutation-testing).
- A pull request says which commands ran and what they printed, from runs
  after the last change.

## Docs

- A change updates the docs, help texts, the user-memory skill and the code
  comments it affects, in the same change. `cargo test --test docs` checks
  that the commands, quoted messages and links in the docs and the skill
  match the code.
- A change users notice gets a line in the next release's notes
  ([releasing](docs/dev/releasing.md)).
- Write plainly: short sentences, no marketing language, no vague claims.

## Git and pull requests

- Open an issue first, describing the problem. The pull request that fixes
  it says `Fixes #N`.
- Branch from current `main`, and rebase onto it before asking for review.
  Name branches by type (`feat/`, `fix/`, `docs/`, `chore/`), one topic each.
- Stage the paths you mean, never `git add -A`. Never rewrite `main` or a
  branch someone else is working on.
- Commit messages follow the rules in
  [CONTRIBUTING.md](CONTRIBUTING.md#what-a-change-includes), with one scope
  per commit. The body says what was wrong and how the change was checked.
- Keep commits separate. Squash only when a later commit undoes an earlier
  one, and check that the squashed tree matches.

## Repository and tooling

- The repository holds only what the project maintains: source, tests,
  configuration and docs written for users and contributors. Plans, notes
  and other working documents stay outside it; the commit guard
  (`.githooks/pre-commit`, with `core.hooksPath` set to `.githooks`) blocks
  the usual ones.
- Everything here is public. Nothing committed may name a private host,
  address, local path or username, or hold a key or token. The release
  check refuses a tree that does
  ([releasing](docs/dev/releasing.md#the-checks)).
- `just` runs every task; the justfile is the authority on what each recipe
  does.
- CI uses the latest stable release of each action and toolchain. A pin to
  an older one says why beside it; `tests/ci.rs` checks the pins.

## Security

Report a vulnerability privately, as [SECURITY.md](SECURITY.md) describes,
with a reproduction, and keep it out of issues, pull requests and commits
until it is fixed.
