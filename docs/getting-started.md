# Getting started

From nothing to a rule that every agent follows, in about fifteen minutes.
You need Claude Code or Codex on this machine, and Docker if you want the
default server.

## 1. Install

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1 | iex
```

The installer asks whether you already have a memory server. Press Enter at
each question to take the default: a server in Docker on this machine. It
then offers search by meaning through Ollama, and asks before it installs or
downloads anything ([choices](install.md#choices-the-installer-offers)).
It ends by running `user-memory doctor`, which prints one line per check:

```output
Binary: OK
…
Claude Code: OK
Claude Code session hook: OK
```

With Codex, the last lines name one step left for you:

```output
Still to do before memory works everywhere:
```

## 2. Restart your agents

Quit and start Claude Code and Codex again, so they load the new connector.
In Codex, run `/hooks` and choose **Trust** for user-memory's two hooks
([why](install.md#approve-the-hooks-in-codex)). `user-memory doctor` then
shows:

```output
Codex session hook: OK (approved in Codex)
```

## 3. Your first session

Open a **new** session in one of your repositories and ask: "what does
user-memory know about you?"

The agent names the conversation's origin, the project the folder loads (its
git remote, such as `github.com/you/app`) and the index of your memories. On
a new server that is:

```output
No user memories recorded yet
```

## 4. Your first rule

Tell the agent something that holds in every project, for example: "Always
ask me before you git push."

A rule for every project changes what agents do everywhere, so the agent
asks whether it holds in every project before it records it. Say yes, and it
is stored as one of your global rules. A rule that holds only in this
repository is recorded for its project instead.

## 5. Check that it landed

```bash
user-memory index
```

lists the rule under `Rules`, with its 8-character id and title. Setup also
wrote it into each client's instruction file, which the client loads at
launch: `~/.claude/rules/user-memory.md` for Claude Code, and a marked block
in `~/.codex/AGENTS.md` for Codex
([your rules in instruction files](how-it-works.md#your-rules-in-instruction-files)).

## 6. Another agent applies it

Start a session in the other client, or on another machine connected to the
same server. Its session start lists the rule. Agents are told to read a
rule in full before they act on it, so before it pushes, the agent asks you
first. If you state the rule again there, the server notices that it is
already stored, and the agent confirms the existing one instead of storing a
copy: the index then shows `×2`.

## 7. Check everything

```bash
user-memory doctor
```

exits with code 0 when all is well, 3 when only steps for you are left, and
1 on an error ([troubleshooting](troubleshooting.md)).

## Next

- [Add another machine](install.md#more-machines), or
  [share one server across your machines](server.md#a-shared-server-behind-https).
- [Choose which memories load in a folder](projects.md).
- [Back up your memories](backup.md).
- [How it works](how-it-works.md), and [all docs](README.md).
