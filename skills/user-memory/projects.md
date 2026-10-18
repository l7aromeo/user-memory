# Projects: which memories load where

A session loads global memories and the memories of every project it sees. The session start's `Projects:` line names each one, grouped by where it comes from, such as `Projects: github.com/x/ws (this folder); listed in .user-memory: github.com/x/api, github.com/x/web; linked: github.com/x/tools.`:

- `(this folder)`: the folder's own project, the one new project memories use.
- `from .user-memory-key, move pending:` a key the folder's key file names besides its git remote; its memories wait for a move (below).
- `listed in .user-memory:` folders the `.user-memory` file lists.
- `linked:` projects this conversation linked.

Pass the keys exactly as the line gives them.

## How a folder gets its project

`user-memory project-id` prints it. Inside a git repository: the `origin` remote, otherwise the first line of `.user-memory-key` at the git root, otherwise the repository's first commit. Outside git: the nearest `.user-memory-key` in the folder or a parent below the home folder. Otherwise the folder has no project, and session start says so and names the repositories directly inside it with their keys, so you can `link` the ones you work in without asking for their remotes.

Two files, one meaning each:

- `.user-memory-key` holds one project key, for a folder without git or without a remote.
- `.user-memory` holds `@<folder>` lines: folders whose projects a session started there loads too, such as the repositories below a parent folder. Any path the user chooses works: relative to the file's folder (`..` may leave it), absolute, or starting with `~/`.

Git ignores both for the user; they are never committed.

## What to do, and when to ask

Both files change what every later session in that folder loads, so write them only through `user-memory project-id`. When the user asks or states the need, such as "always load notebooks and compiler here", "it's annoying to link them every time" or "call this project github.com/acme/app", that is the yes: run the command and report what you wrote. Propose first only when you inferred the need yourself, such as the user linking the same repositories every session without mentioning it, and run it only after their yes.

| Situation | Do | User's yes |
|---|---|---|
| This conversation needs another repository's memories | `link` it with its git remote or key | not needed |
| The user asks or states the need to load the same repositories in every session here, such as "it's annoying to link them every time" | Run `user-memory project-id --include <folder>` for each, then report which `@` lines you wrote | their request or stated need is the yes |
| You inferred the need yourself, such as the user linking the same repositories every session without mentioning it | Propose the `@` lines, then run `user-memory project-id --include <folder>` for each | needed |
| The user asks or states the need for a key for a new project without git or a remote, such as naming its key | Run `user-memory project-id --set <key>`, then report what you wrote | their request or stated need is the yes |
| You inferred yourself that a new project without git or a remote needs a key | Propose a key, such as its planned remote `github.com/<owner>/<name>`, then run `user-memory project-id --set <key>` | needed |
| The header reports a key file that differs from the remote | Ask to run `user-memory project move <file key> <remote key>`, then delete `.user-memory-key` | needed |
| Something about the user that holds in every project | Ask whether it does before recording it with scope global | needed |

- `link` lasts for this conversation, through resume and compaction, and no other. Pass the origin printed at session start. A folder's own project and its listed ones always load there, so linking or unlinking them changes nothing.
- `user-memory project-id --exclude <folder>` removes a listed folder.
- Record what you learn about one repository with scope project and that repository's key from the `Projects:` line, never as global without the user's yes.

## Moving a project's memories

When a repository's key changes (it got a remote after starting under a key file, or its remote moved), its old memories stay under the old key. The session header names both keys and the command:

1. With the user's yes, run `user-memory project move OLD NEW` where the memory store lives. It backs up the store, then re-keys every memory and conversation link of OLD. On a client connected to a server, run it on the server: `docker compose exec memory user-memory project move OLD NEW`.
2. Delete `.user-memory-key` if it named the old key.
3. If NEW already had memories, offer a review with `projects` set to NEW ([review.md](review.md)).
