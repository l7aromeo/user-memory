# Projects

A project memory applies in one project only, such as "use Python in this
repository". A session sees global memories plus the memories of the
projects it loads, and never another project's.

- [Project keys](#project-keys)
- [How a folder gets its project](#how-a-folder-gets-its-project)
- [What a session loads](#what-a-session-loads)
- [A project without git: .user-memory-key](#a-project-without-git-user-memory-key)
- [A folder of repositories: .user-memory](#a-folder-of-repositories-user-memory)
- [Another repository for one conversation: links](#another-repository-for-one-conversation-links)
- [When a project gets a git remote: project move](#when-a-project-gets-a-git-remote-project-move)
- [What needs your yes](#what-needs-your-yes)

## Project keys

A project is named by a key, the same in every clone and on every machine.
For a repository, the key is its `origin` remote in a short form: lowercase
`host/path`, without the scheme, user, port or `.git`. These remotes all give
`github.com/you/app`:

```
git@github.com:You/App.git
https://github.com/you/app
ssh://git@github.com/you/app.git
```

To see a folder's key:

```bash
user-memory project-id
```

## How a folder gets its project

user-memory looks in this order:

1. **A git repository with an `origin` remote:** the remote's key. Nothing
   else is needed.
2. **A git repository without a remote:** the key in `.user-memory-key` at
   the repository's root. Without that file, `git-root:<commit>`, named after
   the repository's first commit. A repository with no commits yet has no
   project.
3. **Outside git:** the key in the nearest `.user-memory-key`, in the folder
   or a parent folder. The search stops below your home folder: a file in
   your home folder, or above it, counts for nothing.

Inside a repository, a file in a parent folder never applies. A folder
outside git without a key file has no project.

## What a session loads

At session start the agent sees one line naming every project the session
loads, grouped by where each comes from:

```output
Projects: github.com/you/notes (this folder); linked: github.com/you/api. Use them as project for scoped memories.
```

| Group | Where it comes from |
|---|---|
| `(this folder)` | The folder's own project. New project memories use it. |
| `from .user-memory-key, move pending:` | A key file that names another key than the remote ([project move](#when-a-project-gets-a-git-remote-project-move)). |
| `listed in .user-memory:` | Folders listed in `.user-memory` ([below](#a-folder-of-repositories-user-memory)). |
| `linked:` | Projects this conversation linked ([links](#another-repository-for-one-conversation-links)). |

A group appears only when it has a project. They load in that order, each
project once. The change notice before each prompt
covers the same projects, and those of every folder the session has worked
in since; it reads them again every time. Moving between folders never drops
a project, but editing `.user-memory`, linking a project or adding a git
remote takes effect at the next prompt, with a first line such as:

```output
Projects changed: - github.com/you/api (no longer listed)
```

When the folder has no project and nothing else loads, the session says so
instead, and tells the agent what to do:

```output
No project: this folder has no git remote and no .user-memory-key. If it holds several repositories, link each one you work in; to load some in every session here, run `user-memory project-id --include <folder>` when the user asks for that, or propose it first. If it is one new project, run `user-memory project-id --set <key>` here when the user names its key, or propose one (such as its planned remote, github.com/<owner>/<name>) and run it after their yes. Record nothing about this work as global without the user's yes.
```

When the folder holds repositories, the line names them with their keys, so
the agent can link or list them without looking for the keys:

```output
No project: this folder has no git remote and no .user-memory-key. Repositories here: compiler (github.com/acme/compiler), notebooks (github.com/acme/notebooks), notes (local:notes). Link the ones you work in; to load some in every session here, run `user-memory project-id --include <folder>` when the user asks for that, or propose it first. If it is one new project, run `user-memory project-id --set <key>` here when the user names its key, or propose one (such as its planned remote, github.com/<owner>/<name>) and run it after their yes. Record nothing about this work as global without the user's yes.
```

It lists the folders directly inside this one that have a project: a git
repository with a remote or with commits, or a folder with a
`.user-memory-key`, each with the key a session there would use. Hidden
folders and folders without a project are left out. After the first 20, by
name, it counts the rest, as in ``and 3 more``.

## A project without git: .user-memory-key

A new project without git, or without a remote yet, has no key. Name it
when you ask the agent to set one, normally the remote the project will
get; when the agent notices the project has none, it proposes a key first.
Either way it runs, in the project's folder:

```bash
user-memory project-id --set github.com/you/notes
```

It prints the key, `github.com/you/notes`, and reports the
[git ignore](#the-git-ignore) on stderr:

```output
Git ignore: .user-memory and .user-memory-key added to <git ignore file>
```

It writes the key, alone, to `.user-memory-key`, and that folder and its
subfolders share the project from then on. The key can also be any stable
name, such as `local:notes`. Outside git the file goes in the folder you
name (default: the current one); in a repository without a remote, at its
root.

`--set` refuses:

- a key that is a local path, such as `~/code/notes`, since a path names the
  project differently on each machine;
- a key starting with `@`;
- a file that already names another key:
  ``<file> already names github.com/you/notes; remove it first to change the project``;
- a different key in a repository that has a remote, since the remote
  already names it;
- a `.user-memory-key` that is a symbolic link, and your home folder or
  above.

Setting the key a file already holds changes nothing.

### The git ignore

`--set`, `--include` and setup make git ignore `.user-memory` and
`.user-memory-key` for you, so neither is ever committed. They add these
lines to your global git ignore (the file `git config --global
core.excludesFile` names, else `~/.config/git/ignore`), once:

```
# user-memory
.user-memory
.user-memory-key
```

Each of these commands reports the ignore in one line on stderr, such as
``Git ignore: <git ignore file> already ignores .user-memory and .user-memory-key``.
If the ignore file is a symbolic link, user-memory leaves it alone and tells
you which lines to add yourself.

## A folder of repositories: .user-memory

If you start sessions in a folder that holds several repositories, list the
ones you work in, so every session started there loads their projects too:

```bash
user-memory project-id --include app
```

```output
Listed @app (github.com/you/app) in <folder>/.user-memory
```

That adds an `@` line to `.user-memory` in the current folder (inside a
repository, at its root). The file holds only `@` lines, blank lines and
`#` comments:

```
# the repositories I work in from here
@app
@api
```

Each listed folder's project comes from its own remote or `.user-memory-key`,
so renaming a remote needs no edit. To remove a line:

```bash
user-memory project-id --exclude app
```

```output
Removed @app from <folder>/.user-memory
```

Both commands keep your comments and other lines as written, and `app` and
`app/` count as one folder. Listing a folder twice, or removing one that is
not listed, changes nothing and says so.

Any path works, as you write it: a folder inside this one, `../infra`,
an absolute path, `~/Projects/infra` or a symbolic link to a folder. One
folder written two of these ways counts once.

`--include` refuses only a path that is not an existing folder, or a
folder with no project, for example:

```output
Error: @web: no project (no commits and no git remote)
Error: @../x: no such folder
```

A listed folder that has no project is not an error at session start: the
agent is told to raise it with you, with a line such as:

```output
Raise with the user once this session: @web: no project (no commits and no git remote)
```

Outside git, the nearest `.user-memory` in the folder or a parent applies,
stopping below your home folder. Inside a repository only the file at its
root counts, so a session started in `app` loads `app` alone.

## Another repository for one conversation: links

When a session needs another repository for a while, the agent **links** it
to the conversation. Its memories then load for the rest of that
conversation, after a resume or compaction too, and on every machine that
uses the same server. Other conversations do not see the link. Linking
needs no yes from you: it changes what loads, not what is stored about you.

Ask your agent, for example: "link github.com/you/api to this conversation".

- The folder's own project, and the ones listed in `.user-memory`, always
  load there; linking or unlinking them changes nothing.
- The agent links by key or git remote URL, never by local path.
- Links need the session hooks, which give the agent the conversation's
  origin.
- Forgetting a conversation removes its links too:

  ```bash
  user-memory forget --origin claude:session/<id>
  ```

  It reports them as `unlinked`.
- Backups (exports) do not include links.

## When a project gets a git remote: project move

A project you started under `local:tool` may later get the remote
`github.com/you/tool`. From then on the remote's key names the project and
new memories use it. The old key still loads, in the header's group
``from .user-memory-key, move pending: local:tool``, and the session start,
`user-memory project-id` and `user-memory doctor` all say what to do:

```output
Project: the .user-memory-key key local:tool differs from the git remote's key github.com/you/tool: run `user-memory project move local:tool github.com/you/tool` where the memory store lives, then delete .user-memory-key
```

1. Move the old key's memories and links to the new key, where the store
   lives:

   ```bash
   user-memory project move local:tool github.com/you/tool
   ```

   It backs the store up first, to a file beside it named
   `<store>.move-<time>.bak`, then reports what it moved:

   ```output
   Backed up <store> to <backup>
   Moved <memories> and <links> from local:tool to github.com/you/tool.
   github.com/you/tool already had <memories>; run `user-memory review --project github.com/you/tool` to merge any duplicates.
   ```

   The counts read like `1 memory` or `3 conversation links`. The last line
   appears only when the new key already had memories.

   For the installer's Docker server, run it there:

   ```bash
   docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory project move local:tool github.com/you/tool
   ```

   On a machine connected to a server it refuses and names the server
   command.

2. Delete `.user-memory-key` from the project folder. After that only the
   remote's key loads.

`project move` refuses the same project twice, a local path, and an old key
that no memory or link uses. When a key file holds the same key as the
remote, it is only redundant, and you are asked to delete it.

## What needs your yes

| Situation | What the agent does | Your yes |
|---|---|---|
| It needs another repository in this conversation | Links it. | Not needed |
| You ask it to load repositories in every session in a parent folder | Runs `project-id --include` for each. | Your request is the yes |
| It notices you keep needing the same repositories there | Proposes the `@` lines first, then runs `project-id --include`. | Needed |
| You name a new project's key | Runs `project-id --set`. | Your request is the yes |
| It notices a new project without git or a remote | Proposes a key first, then runs `project-id --set`. | Needed |
| A key file differs from the remote | Asks you to run `project move`, then to delete `.user-memory-key`. | Needed |
| Something about you that holds in every project | Asks before recording it as global ([global memories](how-it-works.md#scope-global-or-one-project)). | Needed |

Both files change what every later session in that folder loads, so agents
write them only through these commands: when you ask for it, or after your
yes to what they proposed. Either way they tell you what they wrote.
