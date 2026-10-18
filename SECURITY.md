# Security

user-memory holds what your agents know about you, so a flaw that lets
someone else read or change it matters.

## Report a vulnerability

Report it privately, through GitHub's private vulnerability reporting:
open the repository's **Security** tab and choose **Report a vulnerability**
([new advisory](https://github.com/l7aromeo/user-memory/security/advisories/new)).
Please do not open a public issue or pull request for it.

Include a reproduction, so it can be confirmed and fixed:

- the version (`user-memory --version`) and your operating system;
- how it is installed: Docker server, native store, or a client of a
  shared server, and for a server what is in front of it;
- the steps, commands or requests that show the problem, what you expected
  and what happened.

Leave out real memories, access tokens and API keys; a throwaway store
shows the same. The report stays private until a fix is released, and the
release notes say what was fixed.

## Supported versions

Fixes go into the next release; the installers always install the latest
one. Upgrade by running the installer again.

## What is in scope

The server (`user-memory host`), the client and its MCP tools, session
hooks and setup, the installers and uninstallers, and the release builds.
The [privacy notes](docs/privacy.md) say what is stored where and what
leaves your machine: for example, the access token reads and writes every
memory, and the store is not encrypted, so both rely on the files being
readable only by you.
