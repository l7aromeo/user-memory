# Troubleshooting

Start with:

```bash
user-memory doctor
```

It checks the binary, the store or server, each client, the hooks, the skill,
the instruction files and the git ignore. It exits with code:

- 0 when all is well;
- 3 when nothing is broken, but something needs a step from you. That line
  reads ``action needed (below)``, and the step follows, last, on a line
  starting ``Action needed:``;
- 1 on an error, after printing the steps too.

## At session start

| You see | Meaning and fix |
|---|---|
| ``User memory unavailable: …`` | The server could not be reached or refused the request. The session continues without memory. Run `user-memory doctor` for the reason. |
| The agent does not mention user-memory | The session was resumed from before the install or an upgrade. Start a new session. Restart the agent once after an install. |
| The agent lacks a newer tool or option | The session started before an upgrade. Start a new session. |
| ``No project: this folder has no git remote and no .user-memory-key.`` | The folder has no project. Give it a key, list its repositories, or let the agent link them ([projects](projects.md)). |
| ``Raise with the user once this session: @web: no project (no git remote)`` | A folder listed in `.user-memory` has no project. Give it a remote or a `.user-memory-key`, or remove the line with `user-memory project-id --exclude web`. |
| ``from .user-memory-key, move pending:`` | The folder got a git remote after its key was set. Run the [project move](projects.md#when-a-project-gets-a-git-remote-project-move), then delete `.user-memory-key`. |

## From doctor

| You see | Meaning and fix |
|---|---|
| ``Action needed: Codex session hooks: Codex has not approved them yet, so they never run …`` or ``Action needed: Codex session hooks: they changed since Codex approved them, so Codex skips them …`` | Codex runs a new or changed hook only after you approve it: run `/hooks` in Codex and choose **Trust** for both user-memory hooks. Until then memory does not run in Codex. |
| ``Action needed: Skill: … was not installed by user-memory setup; remove it, or run …`` | Another skill named `user-memory` is in the way. Remove it, or keep it and run `user-memory setup --no-skill`, so setup leaves the skill out. |
| ``Action needed: Claude Code: a project-scope user-memory server overrides shared memory in …`` | A Claude Code setting in that folder replaces the shared server there; memory works in every other folder. Run the `claude mcp remove` command doctor prints, in that folder. The same holds for a local-scope one. |
| ``Action needed: Git ignore: … does not ignore …`` | Your global git ignore lacks `.user-memory` or `.user-memory-key`. Run `user-memory setup` again, or add the names it lists, one per line. |
| ``Action needed: Git ignore: core.excludesFile is set to an empty value, so git reads no ignore file …`` | Point `core.excludesFile` at a file, and add `.user-memory` and `.user-memory-key` to it, one per line. |
| ``Action needed: Automatic backups: none yet; run `user-memory backup now` `` | The store holds memories but has no [automatic backup](cli.md#automatic-backups), for example because its `backups` folder was deleted. Run `user-memory backup now` where the store lives. |
| ``Action needed: Automatic backups: the newest is … old though memories changed since; run `user-memory backup now` `` | Memories changed, but no backup was made for over two days. Run `user-memory backup now`; if it fails, the error says why. |
| ``Action needed: Automatic backups: the last one failed (…); fix the cause, then run `user-memory backup now` `` | The last automatic backup failed, for example because the disk is full or the `backups` folder beside the store is not writable. The message in brackets gives the cause. Your memories were stored; only the copy failed. |
| ``Cannot connect to memory server; check endpoint and access token`` | Check the address (`user-memory endpoint`), the token file, and that the server answers: `curl -fsS https://memory.example.com/`. |
| ``… has schema version …; run the installer again …`` | The store needs an upgrade. Run the installer again on the machine that runs the server ([server upgrade](server.md#upgrade)). |
| ``… this user-memory is older than the store …`` | Your client is older than the store. Run the installer again, then restart your agents. |
| ``Database health check failed: …`` | The store is damaged. Run `user-memory repair` where the store lives. It backs the store up, rebuilds its indexes and checks it again. Keep the store on a local disk: a network share (NFS, SMB) cannot share its write-ahead log between processes. |
| ``Action needed: Git ignore: … is a symbolic link; user-memory leaves it and its target as they are …`` | Add `.user-memory` and `.user-memory-key` to the file it names yourself, one per line. Setup says the same when it runs: ``Git ignore: not written …``. |
| ``Git ignore: this repository tracks …; an ignore rule does not untrack it`` | An ignore rule never untracks a committed file. `git rm --cached <file>` stops tracking it and keeps your copy. |
| ``Project: the .user-memory-key key … differs from the git remote's key …`` | Run the `user-memory project move` command it prints, where the store lives, then delete `.user-memory-key`. |
| ``user-memory … is available (this is …): run the installer again`` | A newer release is out. [Upgrade](install.md#upgrade) when it suits you; `user-memory setup --no-update-check` stops the notice. |
| ``Action needed: Store privacy: other accounts on this machine can read …`` | Another account on this computer can read your store, for example a store you placed in a shared folder. Run the command the line names: `chmod 600` on the file on macOS and Linux; on Windows, `icacls` shows who can read it, and the second command it names makes it yours only. |
| ``Action needed: Claude Code … is older than …, which runs user-memory's Windows hooks; run `claude update` `` | Windows only. user-memory's Claude Code hooks need Claude Code 2.1.139 or later, which runs them without a shell. Run `claude update`, then start a new Claude Code session. Setup already wrote the hooks, so you don't need to run it again. |
| ``Cannot read Claude Code's version from `claude --version` …`` | Windows only. Doctor could not tell which Claude Code is installed; the part in brackets, when there is one, shows what `claude --version` printed. Run `claude --version` yourself, and if it fails or shows a version older than 2.1.139, run `claude update`. |
| ``Action needed: Embedding: … is not calibrated …`` | The model was chosen by hand or by an older version, so this store has no duplicate cutoffs for it yet. Run the `user-memory embedding set …` command it prints. |
| ``Duplicate detection: keyword only; … cannot separate the labelled pairs`` | Nothing is broken: this model searches by meaning, but cannot tell duplicates from related memories, so duplicates are caught by their words. Choose another model with `user-memory embedding set` if you want both. |
| ``Project: git cannot read the checkout at …/.git …`` | The folder's git checkout is broken, often a worktree copied from another machine whose `.git` file names a folder that is not there. Repair it with git, or delete that `.git` file if the folder is only a copy. Setup and doctor still run there. |
| The rules were not written to an instruction file | The file is a symbolic link, a `rules/user-memory.md` you wrote yourself, or a Codex file whose markers do not pair up. user-memory leaves such a file alone. The index still lists your rules at session start. |

## When installing

| You see | Meaning and fix |
|---|---|
| ``Added … to your PATH; open a new terminal to use user-memory`` | Windows: terminals opened before the install keep the old PATH. Open a new one. |
| The Windows installer says Windows did not run the downloaded `user-memory.exe` | Smart App Control or an antivirus blocked the unsigned binary. Its SHA-256 matched the release, which the installer checked first. Allow it, or [check the hash yourself](install.md#install-on-windows), then run the installer again. |
| ``Still to do before memory works everywhere:`` | The install worked. The lines below it are steps only you can take, such as approving the hooks in Codex. |
| ``The install finished, but doctor found a problem (above)`` | Something is broken. Fix what doctor printed above, then run `user-memory doctor` again. |
| ``semantic search was not changed (above), though everything else is installed`` | The embeddings endpoint did not work: often Ollama not running, or a model it does not have. Fix what it printed, then run the `user-memory embedding set …` command it names. With the Docker server on Linux, the message also names the Docker bridge address, such as `172.17.0.1`: the server reaches Ollama there, while Ollama listens on `127.0.0.1` by default. Set `OLLAMA_HOST=172.17.0.1:11434` for the Ollama service, restart Ollama, then run the command ([semantic search](server.md#semantic-search)). |

## When upgrading

| You see | Meaning and fix |
|---|---|
| The upgrade stops and lists memories without titles | An upgrade from 0.3. See [titles](install.md#upgrade-from-03-titles). |
| ``… is damaged`` | Nothing was changed. Run `user-memory repair`, then upgrade again. |
| ``… needs schema version … Move it aside`` | The store is too old to upgrade in place. Ask your agent: "upgrade my memory store". The skill walks you through export, a fresh store and import. |

## When uninstalling on Windows

| You see | Meaning and fix |
|---|---|
| A `Left:` line saying the binary is in use by a running agent session | Windows deletes no program a running session uses. Close Claude Code and Codex, then delete the file it names, or run the uninstaller again. |

## Memories

| Problem | Fix |
|---|---|
| The agent recorded something wrong | Ask it to update the memory, or run `user-memory update <full id> --content …`. |
| The same thing is stored twice | Ask: "review my memories". |
| A conversation taught the agent things you do not want kept | Ask: "forget what we discussed in that chat", or run `user-memory forget --origin <origin>`. |
| Search finds only exact words | Semantic search is off. See [semantic search](server.md#semantic-search). |
| The agent asks before recording something as global | Since 0.5 it must. Say yes only if it holds in every project; otherwise ask it to record the memory for this project. |
| A session in a parent folder does not see a repository's memories | List the repository with `user-memory project-id --include <folder>`, or ask the agent to link it ([projects](projects.md)). |
| `record` on the command line says `--scope` is missing | Since 0.5, give `--scope global` or `--scope project --project KEY`. |
