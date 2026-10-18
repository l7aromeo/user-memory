# Contributing

Thank you for helping with user-memory. This page has what every change
needs; [development](docs/dev/development.md) has the detail: the test
layout, the fake programs the tests use, mutation testing and the agent
evaluations.

## Set up

You need Rust 1.99.1 (the Dockerfile's `RUST_IMAGE`, which the checks and
the release builds use), a C build toolchain (SQLite is bundled),
[just](https://github.com/casey/just) and
[cargo-nextest](https://nexte.st/):

```bash
cargo install cargo-nextest --locked
git config core.hooksPath .githooks
```

The second line turns on the commit guard, which keeps plans and designs
out of the repository.

## Check a change

```bash
just check
```

It runs `cargo fmt --check`, clippy with warnings as errors, and every test.
A pull request must pass it on macOS, Linux and Windows; CI runs it on each.
Some tests need `shellcheck`, `pwsh` with PSScriptAnalyzer, or Docker, and
skip without them locally.

## What a change includes

- **Tests.** New behaviour comes with a test, and a fix with a test that
  fails without it. One case per distinct way it can fail.
- **Docs.** A change updates the docs and code comments it affects, in the
  same change. `cargo test --test docs` checks that documented commands
  and quoted messages match the code.
- **Release notes.** A change users notice gets a line in the next release's
  notes, `release-notes/v<version>.md`, when it is prepared
  ([releasing](docs/dev/releasing.md)).
- **Commits.** [Conventional Commits](https://www.conventionalcommits.org/):
  an imperative subject under 70 characters, such as `fix(setup): keep the
  user's hook order`, and a body that says why.

## What users rely on

- The CLI, the MCP tools, the files on disk and the store format are
  published. Change them only in a planned release, with upgrade notes; a
  renamed command or field keeps a deprecated alias for at least one minor
  release.
- The store holds real memories. A change to its schema (tables, triggers,
  search-index SQL) bumps `SCHEMA_VERSION`, adds a schema snapshot and
  ships a tested forward migration from the previous version, which setup
  runs after an automatic backup. Never drop or rewrite stored memories to
  simplify a change.
- `unsafe` code lives only in `src/platform/windows.rs`, each block with a
  `// SAFETY:` comment. Private files go through `src/private.rs` and
  per-OS locations through `src/paths.rs`.

## Security issues

Report them privately, as [SECURITY.md](SECURITY.md) describes, not in an
issue.

## Licence

Unless you say otherwise, a contribution you submit for inclusion is
licensed as the project is, under MIT OR Apache-2.0, without any additional
terms or conditions.
