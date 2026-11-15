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

`--dry-run` prints the sessions planned per scenario and client, the model
each client will use, whether the eval logins are in place, and what the
guard watches. `--rehearse` installs
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
`--binary [LABEL=]PATH` chooses the binary under test (default
`target/debug/user-memory`), labelled in the report by `LABEL`, such as its
commit, or by its file name. Given more than once, the batch runs on each, as
a branch beside its base, in one report. `--dry-run`, `--rehearse` and the
report show each binary's path and SHA-256, and a run stops before its first
session when two of its binaries have the same contents: that compares one
build with itself.

## Running a batch

1. Build the binary under test from a clean export, outside `/tmp` for the
   same reason as the trials, and pass it with `--binary`. Reuse the one `build`
   folder for every commit: the export is replaced, so no file deleted
   since an earlier commit is built, and the target folder rebuilds only
   what changed. `tar -m` gives the files the time they were extracted:
   with the commit's time, which `git archive` sets, cargo takes a commit
   older than the last build as built already, and the binary is the
   previous commit's. Copy each binary out, and check it holds a text only
   that commit has:

       B=~/Library/Caches/user-memory-eval/build
       rm -rf $B/export && mkdir -p $B/export && git archive <commit> | tar -x -m -C $B/export
       (cd $B/export && CARGO_TARGET_DIR=$B/target cargo build --locked)
       mkdir -p $B/bin && cp $B/target/debug/user-memory $B/bin/user-memory-<commit>

   To run a branch beside its base, pass both:
   `--binary 67019a2=$B/bin/user-memory-67019a2 --binary <commit>=$B/bin/user-memory-<commit>`.
2. Create the two eval logins (below) if they are missing; `--dry-run` says.
3. Quit the ChatGPT and Codex apps for the whole batch: their background
   app-server writes the real `~/.codex`, which stops the batch. Check that
   `pgrep -fl "ChatGPT.app|Codex.app|app-server daemon"` finds nothing.
4. `--dry-run` and `--rehearse`, then one scenario as a smoke test, then the
   batch.

Trials run keyword-only unless `USER_MEMORY_EVAL_EMBED_URL` names an
embeddings endpoint. Then `USER_MEMORY_EVAL_EMBED_MODEL`, `_DIMENSIONS` and
`_QUERY_INSTRUCTION` go with it, and each trial's setup saves them as its
`USER_MEMORY_EMBED_*` settings, so the agents search semantically. Plain http
is allowed only for an endpoint on this machine. `--dry-run` shows which
mode a batch would use:

    USER_MEMORY_EVAL_EMBED_URL=http://127.0.0.1:11434/v1/embeddings \
    USER_MEMORY_EVAL_EMBED_MODEL=qwen3-embedding:4b USER_MEMORY_EVAL_EMBED_DIMENSIONS=1024 \
    evals/run.py --dry-run

`--budget` caps the running total in `run-count`, not one batch: a batch
starts only if the sessions already counted plus those it plans stay within
it. After a 4-session smoke test, a 31-session batch needs `--budget 35`.

## The fake model

`evals/fake_model.py` stands in for the Anthropic Messages API and the
OpenAI Responses API, so the real Claude Code and Codex can run a turn that
bills nothing. It listens on 127.0.0.1 only, forwards nothing and answers
each request from a script: a text, a tool call, which can wait for the
test to release it so the test can act during a turn, or a refusal with an
HTTP status, to see how a client ends a failed turn. It keeps each
request's path and JSON body, never its headers, so no token a client sends
reaches its record or its log file, which a caller keeps in the trial
folder. Every reply reports the same usage (`USAGE`). `python3 -I
evals/test_run.py` checks it with plain HTTP, so it needs neither client.

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
- Each batch runs the cheaper model of each provider's latest family: Claude
  Code's `sonnet` alias, which it resolves to the newest Sonnet, and Codex's
  `gpt-6-luna`, at `high` effort. A pass on a weaker agent says more about
  memory, and the batch costs less. `--top-models` runs Claude Code's `opus`
  and Codex's top tier instead; a failure on the cheaper models is run again
  there before it is called a bug. Codex's top tier needs a login whose
  plan serves that model; on a plan that doesn't, the session fails and the
  step's `error` says so. `USER_MEMORY_EVAL_CLAUDE_MODEL`,
  `USER_MEMORY_EVAL_CODEX_MODEL` and `USER_MEMORY_EVAL_CODEX_EFFORT` name
  others for one run. The Codex tiers are named once in `evals/run.py`; every
  run except `--list` asks the installed Codex for its model list and stops
  when it reports one of them, or the model chosen, superseded.
- Each trial records the tokens its sessions used: Claude Code's `result`
  events (its `modelUsage`, with Claude Code's own cost estimate) and Codex's
  `thread/tokenUsage/updated` notifications (priced from `CODEX_PRICES` in
  `evals/run.py`). The batch summary shows input, cached input, output,
  reasoning and estimated cost per scenario, client and model.
- A turn the client ends as failed is the step's `error`: Claude Code's
  `result` marked `is_error`, or a Codex turn that ends `failed` or an
  `error` it won't retry, with the reason it gave. Such a trial prints as
  `ERROR`, not `FAIL`, since its checks graded no choice of the agent's,
  and the summary counts it as errored beside the passed runs.

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

Each scenario may `seed` memories through the CLI, in order (seed `id`s are
referenced by checks, and a seed's `args` may name an earlier seed as
`{seed:NAME}`, its full id, so `"--supersedes", "{seed:old}"` replaces it;
`"rule": true` marks a seed the rehearsal expects in the instruction file,
and `"rule": false` one it expects absent from it),
then runs its `sessions` (one turn each) or `steps` (turns
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

`files` maps trial paths to text written before the first session, in
private folders; a file inside a project checkout joins the project's first
commit, so `commits` checks count only what the agent committed, and
one in `exports/` is in the trial store's exports folder. `servers` names
`user-memory host` servers, each on a loopback port with its store, token
and log in the trial's `servers/<name>/`; they start before the first
session and stop after the last. The client connects to the one `connect`
names (by default the only one), whose store takes the seeds, with its own
copy of the token. A step's `server` acts on them before its turn: `stop`
or `start` a server, or `refuse`, which gives the server a new token, so
it refuses the one the client holds (a bridge keeps the token it started
with, so only the server's side can refuse it mid-session). `versions`
makes the debug build report another release, as `USER_MEMORY_TEST_VERSION`
does: `client` for the trial's client and what its agent app starts, a
server's name for that server, such as a client of a newer release than its
server. `client_token` gives the client its own token from that server, as
its operator would with `user-memory host token add`, instead of a copy of
the shared one: `{"read_only": true}` makes it read-only.

A step's `while_working` is a second user message, sent once the agent's
first tool call in the turn shows, as a user types while the agent works:
Claude Code adds it to the running turn, and Codex gets it through
`turn/steer`. A step's `answers` (`{"question": REGEX, "pick": REGEX}`, or
a list of them) say how the user answers the client's question tool: each
question takes the first entry whose `question` matches, and the option
whose label `pick` matches; a question with no such entry or option is
declined. A scenario with `answers` gives Claude Code its question tool
(`AskUserQuestion`), answered by the harness as host for its prompts
(`--permission-prompt-tool stdio`); every other call no allow rule covers
is declined there, as Claude Code declines it with no one to ask. Codex has
no question tool in its default mode, so such scenarios run on Claude Code
only.

`--rehearse` first runs one turn of each installed client against the
[fake model](#the-fake-model), through the harness's own session code, and
fails if a client no longer does what the harness relies on: take a message
during a turn (`while_working`), report usage, say why a turn failed (one
turn whose request the fake model refuses), and for Claude Code also let
the harness answer its question tool and show the server instructions to
the model. Whether Codex has a question tool in its default mode, and
whether it shows the server instructions, is reported, not required. A
client that isn't installed is skipped with one line.

`rehearse` entries add checks to `--rehearse` that need no agent: each runs
the trial's installed session-start hook in its `cwd` with a made-up payload
(`source` defaults to `startup`), after linking `links` to that made-up
conversation, and requires every `matches` pattern in the header it prints
and no `lacks` pattern.

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
- `commits`: a commit landed after the project's first one, or with
  `matches`, one whose message matches (`^` is the start of its subject).
- `commits_not`: no commit after the project's first one matches.
- `approvals_not`: no escalation Codex asked for matches, whatever the
  answer; a declined push request still counts as an attempted push.
- `file`: a trial file (`path` relative to the trial folder) matches, or is
  `absent`; after a step's turn (`"step": N`, from `watch`) or at the end.
- `instructions`: the client's instruction file matches.
- `any`: a list of checks; passes when one of them does, for an outcome
  reached two ways, such as "the tests ran, or nothing was committed".
- `question`: the step asked through the client's question tool (with
  `matches`, a question whose text matches).
- `reads_not`: no step read a file whose path matches with Claude's file
  tools; Codex reads through shell commands, which `commands_not` checks.
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
only when named. Some diagnostics are expected to fail: they seed only the
memory that should lose, to show another scenario's checks can fail.
