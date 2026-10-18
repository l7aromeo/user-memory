# Local repository guidance

- Never commit implementation plans, temporary designs or progress logs.
- Write plans outside this repository, for example in a temporary folder.
- Keep maintained setup, operation, configuration and reference documentation. `README.md`, `CONTRIBUTING.md`, `SECURITY.md`, the user guides in `docs/` and the contributor guides in `docs/dev/` belong in the repository.
- user-memory is published: its CLI, MCP tools, files on disk and store format are what users rely on. Change them only in a planned release, with upgrade notes and a tested migration for stored data; a renamed command or field keeps a deprecated alias for at least one minor release before it is removed.
- The memory store holds real user data. Every change to the stored schema (tables, triggers, search-index SQL) bumps `SCHEMA_VERSION`, adds a schema snapshot, and ships a tested forward migration from the previous version; `setup` runs it after an automatic backup. Never drop or rewrite stored memories to simplify a change.
- The local commit guard is `.githooks/pre-commit`; keep `core.hooksPath` set to `.githooks`.
- `unsafe` code lives only in `src/platform/windows.rs`; the crate denies `unsafe_code` everywhere else. Each `unsafe` block there carries a `// SAFETY:` comment. Private files go through `src/private.rs` and per-OS locations through `src/paths.rs`, never mode bits or `HOME` at the call site.
