# Agent evaluations

`evals/run.py` runs real Claude Code and Codex sessions against the built
`user-memory` binary, installed the way a user installs it, and grades what
the agents did with memory: what they recorded, read and forgot, whether they
followed the user's rules, and what they left in their own files. Scenarios
live in `evals/scenarios.json`.

Every session is a billed model run. Check the plan and rehearse the setup
first; neither runs a session:

    cargo build --locked
    evals/run.py --list
    evals/run.py --dry-run
    evals/run.py --rehearse
    evals/run.py --scenario codex_records_through_user_memory --client codex
    evals/run.py --budget 100

`--dry-run` prints the sessions planned per scenario and client, whether the
eval logins are in place, and what the guard watches. `--rehearse` installs
one trial per scenario and client and checks what setup wrote (instruction
files with the seeded rules, both hooks or none, the MCP registration, and
Codex's approval of the hooks), plus the session-start header a scenario's
`rehearse` entries describe. `--budget` refuses a batch that would take the
total past that many sessions; the total is kept in
`$USER_MEMORY_EVAL_OUT/run-count`. Reports, per-trial JSON with raw event
streams, and the guard log go to `$USER_MEMORY_EVAL_OUT` (default
`/tmp/user-memory-evals`), never into the repository: the harness
refuses an output folder inside it. The trials themselves (homes, stores,
binaries, projects) go to `$USER_MEMORY_EVAL_TRIALS` (default
`~/Library/Caches/user-memory-eval/trials`), one 0700 folder per batch:
outside `/tmp`, `TMPDIR` and the repository, which the harness refuses,
because Codex's workspace sandbox may write anywhere in `/tmp` and `TMPDIR`
and could otherwise plant a program a trial later runs outside it. A batch
holds whole homes, binaries and projects, so each new batch keeps only the
newest 3 batch folders there, itself included, and removes the older ones;
`USER_MEMORY_EVAL_KEEP_TRIALS=N` keeps N. Only folders named like a batch
(`YYYYMMDD-HHMMSS`) are removed; anything else there stays.
`--binary` chooses the binary under test (default
`target/debug/user-memory`).

## Running a batch

1. Build the binary under test from a clean export with its own target
   directory, so no stale build is tested, outside `/tmp` for the same
   reason as the trials, and pass it with `--binary`. Reuse the one `build`
   folder for every commit: the export is replaced, so no file deleted
   since an earlier commit is built, and the target folder rebuilds only
   what changed:

       B=~/Library/Caches/user-memory-eval/build
       rm -rf $B/export && mkdir -p $B/export && git archive <commit> | tar -x -C $B/export
       (cd $B/export && CARGO_TARGET_DIR=$B/target cargo build --locked)

2. Create the two eval logins (below) if they are missing; `--dry-run` says.
3. Quit the ChatGPT and Codex apps for the whole batch: their background
   app-server writes the real `~/.codex`, which stops the batch. Check that
   `pgrep -fl "ChatGPT.app|Codex.app|app-server daemon"` finds nothing.
4. `--dry-run` and `--rehearse`, then one scenario as a smoke test, then the
   batch.

`--budget` caps the running total in `run-count`, not one batch: a batch
starts only if the sessions already counted plus those it plans stay within
it. After a 4-session smoke test, a 31-session batch needs `--budget 35`.

## Logins

Trials never use the user's own logins. Create two dedicated ones once:

- Claude Code: run `claude setup-token` and save the token in
  `~/.local/share/user-memory-eval/claude-token` with mode 0600
  (`USER_MEMORY_EVAL_CLAUDE_TOKEN_FILE` names another file). The harness
  refuses a missing file or a wider mode, and passes the token only as
  `CLAUDE_CODE_OAUTH_TOKEN` in a session's environment; it is never printed,
  logged or written to a report.
- Codex: `CODEX_HOME=~/.local/share/user-memory-eval/codex-auth codex login`
  (`USER_MEMORY_EVAL_CODEX_AUTH` names another folder). Each Codex session
  gets a symlink to its `auth.json` in the trial's own `CODEX_HOME`, removed
  when the session ends.

## Isolation

Each trial gets its own `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
`XDG_STATE_HOME`, store and copy of the binary, and a minimal environment
(those, `USER`, `LANG`, `TMPDIR` and a `PATH` holding the system tools first,
then the trial's binary and the client CLI, so `git` is always the system
one; the harness itself runs git and user-memory by absolute path). Seeds are recorded first; then
`user-memory setup --client <client>` installs the trial exactly as a user
would: instruction files with the user's rules, both hooks (or none, with
`"hooks": false`), the skill and the MCP registration. For Codex the trial's
hooks are then approved the way a user approves them in `/hooks`: their
current hashes are recorded as trusted in the trial's `config.toml`.

- Claude Code runs `claude -p` with stream-json input and output, reading
  only the trial's user settings (`--setting-sources user`): its hooks, rules
  and MCP server. It has no built-in tools unless a scenario gives them, and
  the memory tools are allowed. In a `shell` scenario the allow rules are
  `Edit(//<project>/**)` for the session's project folder only (Edit rules
  cover Write; the path is resolved, as an allow rule needs the path and
  what it resolves to both to match) and exact git shapes: `git status`,
  `git status --short`, `git add .`, `git add -A`, `git add <file>` for each
  file the project tracks, and `git commit -m *` (git refuses `-F` next to
  `-m` and ignores `--template`). Deny rules are a backstop: `git push`,
  `--output`, the file and template options of a commit, `--pathspec`, and
  edits in the trial's `bin`, `home`, `tools` and `state`. Everything else,
  reads outside the working directory included, stays unapproved and so
  refused. Claude runs with `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`, which
  strips credentials, the eval token among them, from every command, hook
  and MCP server it starts.
- Codex runs `codex app-server` with the trial's `CODEX_HOME`: one ephemeral
  thread, plugins and web search off, no shell unless a scenario needs one,
  no approval asked for unless a scenario allows escalations (below), and
  the memory tools approved. The thread's `instructionSources` and the
  context hooks add are kept in each step's record.
- The Codex CLI is the one on `PATH`, else one inside ChatGPT.app or
  Codex.app. It is resolved once per batch, linked into every Codex trial's
  `PATH` so setup uses the same build, and fingerprinted by the guard, so an
  update during the batch stops it. Claude trials run with
  `DISABLE_AUTOUPDATER=1`, so the installed Claude Code cannot update itself
  mid-batch.
- A session stays open between its turns (`steps` in a scenario), so one
  session can keep running while another changes memory.
- Projects are git checkouts with an origin remote; a push goes to a local
  path that does not exist, so it never leaves the machine. Codex's sandbox
  keeps the git directory read-only, so a scenario with `codex_escalations`
  lets Codex ask to run a command outside it (approval policy `on-request`).
  The harness approves a command only if it runs in the trial and every
  step joined by `&&` or `;` is a git command with options from an
  allowlist: `status --short/-s/--porcelain/-b`, `diff
  --stat/--cached/--staged/--name-only/--check`, `log --oneline/--stat/-N/-n
  N/--format=`, `show --stat/--name-only/--oneline/--format=` with a
  revision, `add -A/-u` with paths inside the trial, and `commit` with only
  `-m`/`--message`; at least one step adds or commits. A command holding
  anything the shell would expand (`$`, a backtick, `~ { } * ? [ ] !`, or a
  word starting with `=`), a pipe, a redirection or any other option is
  declined, and so is everything else. `python3 evals/test_run.py` checks
  the policy. Each request and its decision is kept in the step's
  `approvals`.
- Models are the user's defaults, passed explicitly because trials never read
  the user's settings: `USER_MEMORY_EVAL_CLAUDE_MODEL` (default `opus[1m]`),
  `USER_MEMORY_EVAL_CODEX_MODEL` (default `gpt-6.1-sol`) and
  `USER_MEMORY_EVAL_CODEX_EFFORT` (default `high`).

The guard fingerprints the Codex binary in use, the user's real memory
store, installed binary and receipt, `~/.claude/settings*.json`, `~/.claude/CLAUDE.md`,
`~/.claude/rules/user-memory.md`, `~/.codex/config.toml`, `hooks.json`,
`AGENTS.md`, `AGENTS.override.md` and `auth.json`, and the MCP server entries
of `~/.claude.json`, before the first session and after every session, and
aborts the batch on any change. It also aborts if a snapshot named after a
trial's session appears under the real `~/.local/state/user-memory/sessions`,
or a folder named after a trial path appears under the real
`~/.claude/projects`. A trial that fails partway still closes every session
it opened, and removes the Codex login link.
Only the MCP entries of `~/.claude.json` are compared because running Claude
Code sessions rewrite its caches and counters continuously. Recording a
memory in a real session during a batch rewrites the real instruction files
and so stops the batch.

## Scenarios

Each scenario may `seed` memories through the CLI (seed `id`s are referenced
by checks; `"rule": true` marks a seed the rehearsal expects in the
instruction file), then runs its `sessions` (one turn each) or `steps` (turns
naming their `session`, kept open until its last turn) in a working directory
(`cwd`: `plain`, `folder:<path>` without git, or `project:<path>`, a checkout
whose remote is the project in `projects` named by the path's last part, so
`project:workspace/notebooks` is a checkout of notebooks inside the folder
`workspace`). `folders` lists such folders to create before the first session,
so a session in a parent folder finds its repositories there. `repeat` and
`clients` set how many trials a batch runs. `shell` gives the agent git and
file edits (Claude: no push; Codex: workspace-write sandbox);
`claude_file_tools` gives Claude Read, Write and Edit and turns its auto
memory on, so its own memory files compete with user-memory. Writes stay
unapproved, so the check sees the attempt. `commands` lets the agent run only
the listed user-memory commands (`"user-memory project-id"`: Claude may run
it by name or by the trial's installed path; Codex gets its workspace-write
shell).

A step may instead `compact` its session (Claude: `/compact`; Codex:
`thread/compact/start`), or `resume` it: the session's process ends and a new
one continues the same conversation (Claude: `--resume`; Codex:
`thread/resume`), as a user resumes it the next day. A resumed session counts
as one more session against the budget. Prompts after the first are the
user's scripted replies: a "yes" appears only where the scenario says the
user agrees, so checks on the turn before it tell asking apart from doing.
`watch` names trial files (relative to the trial folder) whose contents are
kept after every step, for `file` checks on one step.

`rehearse` entries add checks to `--rehearse` that need no agent: each runs
the trial's installed session-start hook in its `cwd` with a made-up payload
(`source` defaults to `startup`), after linking `links` to that made-up
conversation, and requires every `matches` pattern in the header it prints.

Checks grade the final store (read with `search --scope all --status all`)
and each step, by default the last (`"step": N`). In any string,
`{seed:NAME}` is a seed's short id and `{memory:REGEX}` the short id of the
memory whose title or content matches.

- `memory`: active memories (every status with `"status": "all"`) matching
  `where` (field regexes) number `count` (default `>=1`), and every match
  satisfies `fields` (regex, or `nonempty`).
- `seed`: a seeded memory is `absent`, or has `fields`.
- `answer` / `answer_not`: the step's final answer does or does not match.
- `called` / `not_called`: the step called one of `tools` with matching `args`.
- `real_origin`: the step recorded with the conversation's real origin.
- `commands` / `commands_not`: a shell command of the step (or of every step,
  `"step": "all"`) does or does not match.
- `commits`: a commit landed after the project's first one.
- `commits_not`: no commit after the project's first one matches.
- `approvals_not`: no escalation Codex asked for matches, whatever the
  answer; a declined push request still counts as an attempted push.
- `file`: a trial file (`path` relative to the trial folder) matches, or is
  `absent`; after a step's turn (`"step": N`, from `watch`) or at the end.
- `instructions`: the client's instruction file matches.
- `hook_context`: what hooks added to the context during the step matches
  (Codex reports it with each hook run).
- `no_client_memory`: no step tried to write a file with Claude's own file
  tools, and nothing landed in the trial's auto memory folders.

Every Codex trial also reports, as informational, whether Codex wrote to its
own memory databases (`memories_*.sqlite` in the trial's `CODEX_HOME`), which
compete with user-memory as Claude's auto memory does: the rows of each table
before the first session and after the last.

Checks marked `informational` are reported but do not decide the trial; a
check may name the `client` it applies to. Scenarios marked `diagnostic` run
only when named.
