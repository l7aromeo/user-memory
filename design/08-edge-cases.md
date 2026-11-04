# 08. Edge cases

One matrix of every scenario the accounts brief asked about and every edge
case the issues list, with the outcome and where it is designed. Messages
are quoted as the issues define them. "Same as today" means main plus
v0.10.0.

## The brief's ten scenarios

| # | Scenario | Outcome | Issue | Detail |
|---|---|---|---|---|
| 1 | A server turns accounts on while it holds memories written with the shared token | `--owner` is required; the owner gets the existing store, which keeps its path. There is no unclaimed state. The shared token keeps working as the owner's until `host token retire-shared`. | #63, #48 | [04](04-accounts-mode.md) |
| 1a | Clients still on the shared token, mid-session | Keep working, as the owner. | #63 | [07](07-state-machines.md) §6 |
| 1b | The same clients at their next session start | Once: ``Memory server work now has accounts; this machine still uses its token from before accounts, which acts as github:alice. To use your own account, run `user-memory login`.`` | #63 | [07](07-state-machines.md) §6 |
| 1c | The same clients after `retire-shared` | Refused, with #48's revoked message naming `user-memory login`. | #48 | [07](07-state-machines.md) §4 |
| 2 | A server turns accounts off again with several users | `disable` lists each user's count and last activity, offers each an export, asks at a terminal, then serves the owner's store or the `--keep` user's. The others are dormant, never merged or deleted. | #63 | [04](04-accounts-mode.md) |
| 2a | A dormant user's client | `Memory server work turned accounts off; its operator keeps your memories until accounts are on again. Ask them for an export.` | #63 | [07](07-state-machines.md) §5 |
| 2b | Accounts turned on again | Dormant users are served again. | #63 | [07](07-state-machines.md) §5 |
| 3 | A person signs in for the first time on a server with accounts | Allowed by a rule: a new user and store at `users/<store id>/`. Refused: `github:bob may not sign in to memory server work; ask its operator to allow you (on the server: user-memory host allow add user:github:bob)`. | #63, #64 | [03](03-identity-and-auth.md) |
| 3a | They had a local store before | Kept apart until they copy it: `user-memory remote copy NAME` (name proposed) previews, asks, imports as an undoable import, and leaves the local store as it was. | #97, #69 | [11](11-skill-docs-evals.md) |
| 4 | One machine switches accounts (GitHub to GitLab, or another person) | `login` as another account needs confirmation: the terminal, a native dialog, or without a display the phrase `sign memory in as github:bob`. Rules are cleared, then filled from the new account. Snapshots are keyed by server key and user id, so nothing leaks across. | #62, #61 | [06](06-confirmation-and-approval.md) |
| 5 | One person with a GitHub and a GitLab identity | One user if linked (`account link`), two if not. Linking never goes by email. Two existing accounts join with `account merge`, recorded as an import that `forget --import ID` undoes. | #67, #43 | [03](03-identity-and-auth.md) |
| 6 | Switching servers while sessions run | Running sessions keep their server, with a notice once; new sessions use the new one. | #61 | [07](07-state-machines.md) §2 |
| 6a | The target is offline | Switches anyway (the key is pinned): `home is unreachable now; new sessions will say so until it answers`. | #61 | [05](05-remotes-and-switching.md) |
| 6b | The target runs another version | `home runs user-memory 0.9.1; this client needs 0.11 or newer. Update the server, or keep using work.` | #61 | [05](05-remotes-and-switching.md) |
| 6c | The target's key changed | Refused, nothing sent; #58's message with the `remote trust` command. | #58 | [03](03-identity-and-auth.md) |
| 6d | The target needs a sign-in, the current one doesn't | `remote use` runs `login` inline and switches only if it succeeds. | #61 | [05](05-remotes-and-switching.md) |
| 6e | The other way round | Switches; the new server needs no sign-in. | #61 | [05](05-remotes-and-switching.md) |
| 6f | Switching back | The same command; the pin, sign-in and snapshots are reused. | #61 | [05](05-remotes-and-switching.md) |
| 6g | What moves with a switch | The rules block is cleared and refilled with a header. `.user-memory`, `.user-memory-key`, the embedding setting and conversation links stay as they are. | #61 | [05](05-remotes-and-switching.md) |
| 7 | One machine's token lost or revoked | `account clients revoke laptop` (or `host token revoke`); the other machines keep working. | #48, #67 | [07](07-state-machines.md) §4 |
| 7a | An account removed by the operator | Tokens revoked, sign-in refused, store to `removed/`. `restore` brings it back; only `purge` deletes. | #63 | [07](07-state-machines.md) §5 |
| 8 | Backups on a multi-user server | Per store, as today. The registry has its own daily backups. The operator restores; users export at any time. | #63, #70 | [04](04-accounts-mode.md) |
| 8a | Importing someone else's export | Allowed, with a preview line: `This export came from github:bob on memory server work; importing adds its memories to github:alice.` | #69 | [04](04-accounts-mode.md) |
| 9 | A client older than accounts, on a server with them | Works through the shared token while it lives; after that, today's message. | #48 | [07](07-state-machines.md) §4 |
| 9a | A server older than the client | `remote use` refuses on the version gap; a 401 without `error_description` gives today's message. | #61, #48 | [05](05-remotes-and-switching.md) |
| 9b | A native install that never enables accounts | No change: same files, schema, registrations, messages and doctor lines, apart from doctor's server line. A test checks no new file appears. | #56, #60 | [00](00-overview.md) |
| 10 | The OAuth callback behind a proxy | Redirect URIs come only from `--public-url`, never from `Host` or `X-Forwarded-*`. doctor and `host accounts status` name a dropped path. | #64 | [03](03-identity-and-auth.md) |
| 10a | A stolen client token | Revoked by name; `account clients` shows each token's last use. | #48, #67 | [09](09-security.md) |
| 10b | An agent switching, signing in or saving a rule without the user | Needs a terminal, a native dialog the user answers, or without a display the user's own next prompt (#62); a rule needs the Save dialog (#53); the skill forbids acting alone (#72); evaluations check it (#74); the next session start shows any switch and new rule. | #53, #62, #72, #74 | [06](06-confirmation-and-approval.md) |
| 10c | A malicious server during a switch or join | The key check fails and nothing is sent; a connect link carries the fingerprint. | #58, #68 | [03](03-identity-and-auth.md) |

## Server keys and pinning (#58)

| Case | Outcome |
|---|---|
| A server with no key (older server) | Recorded as having no key. Once a key has been seen, its absence is a mismatch. |
| A restored data folder | Keeps the key; nothing changes for clients. |
| A loopback development server | Gets a key too. |
| A relay at another URL | Fails: the URL is in the signature. |
| A planned rotation (`host key rotate`) | Both keys served for 30 days; clients re-pin once and say so. |
| A compromised key (`host key rotate --no-chain`) | Every client sees a mismatch. |
| The check is too slow for `PROMPT_LIMIT` | Cache the result per session id, never per process. |

## Remotes and switching (#60, #61, #71)

| Case | Outcome |
|---|---|
| Two names for one server | `add` warns: `"home" is the same server as "work" (same key)`. |
| A domain move | `remote set-url`, accepted only because the new URL proves the pinned key. |
| Environment setups and CI | `USER_MEMORY_URL` with `USER_MEMORY_TOKEN_FILE` (plus `USER_MEMORY_SERVER_KEY`), then `USER_MEMORY_DB`, keep their meaning and priority. |
| The first setup after the upgrade | The receipt's remote becomes `default`, keeping its token-file path, pinned and active; registrations are rewritten without the baked variables. |
| A local-only install | No `remotes*` file until a `remote` command runs. |
| `local` as a remote name | Reserved for this machine's store. |
| Removing the active remote | Refused. |
| Claude Code and Codex on one machine | One switch covers both. |
| The new server unreachable at the next session start | `User memory unavailable: …`, with the reason. |
| Uninstall, server reachable | Token revoked (`DELETE /auth/token`), file deleted. |
| Uninstall, server unreachable | ``Left: this machine's token for remote work (memory.example.com could not be reached); revoke it from another machine with `user-memory account clients revoke laptop` `` and exit 1. |
| Uninstall on a server without accounts | A client token is revoked the same way; the shared token file is left, as today. |
| `uninstall --dry-run` | Lists what it would do and changes nothing. |

## Confirmation and rules (#53, #62)

| Case | Outcome |
|---|---|
| A covered command at a terminal | Asks there, `[y/N]`. |
| No terminal, a display | A native dialog; Cancel or no answer within `CONFIRM_DIALOG_WAIT` changes nothing: ``Not changed: the confirmation was cancelled.`` |
| Cancel for `login` | The held token is revoked at once. |
| No terminal and no display | The typed phrase, as the user's whole next prompt. |
| The phrase inside a longer message | Doesn't count. |
| A wrong, expired or reused phrase | Refused; an expired request is deleted at the next hook run. |
| An expired `login` request | The hook marks the held token lapsed; the next command that reaches the server, or the next session start, revokes it. |
| A prompt that isn't the user's (automation, `codex exec`, another session) | Ignored where the client marks it; the docs say where a client can't tell. |
| A client without a prompt hook, no display | Terminal only. |
| Two sessions open, the phrase typed in the other one | Applies: the request is per machine and OS user, and the phrase is the user's. |
| An agent records an explicit preference | `serve` asks ``Save as your rule: …?``; Cancel stores nothing and the agent gets ``The user chose not to save this rule.`` |
| An agent records an observation or a project fact | Never asks. |
| An agent turns a memory into a rule with `update` | Asks, as for a record. |
| A rule recorded over SSH | Stored tentative; the next `serve` with a display asks; on a machine that never has one, #42's block lists it with `user-memory update ID`. It never lapses. |
| A rule typed by the user at a terminal | The user's own yes; no dialog. |
| An agent's shell runs `user-memory record` for a rule | No terminal, so the dialog asks. |
| An agent calls the server directly with the machine's token | Not prevented: it skips `serve`. Read-only tokens (#48) close it for clients that don't write. |
| A program running as the user editing `remotes.json` or driving a dialog | Not prevented; visible at the next session start. |

## Accounts and stores (#57, #63, #69, #70)

| Case | Outcome |
|---|---|
| Enabling on an empty server | `--owner` optional. |
| A store several people shared | Goes whole to the owner, who can give others filtered exports. |
| The operator is also a user | Signs in like everyone else. |
| A restored memory store | The registry is untouched; revoked tokens and removed users stay so. |
| A restored registry | Every client token revoked; users, stores and allow rules come back as they were. |
| One store, as today (#57) | Same behaviour and performance; measured with `just search-budget` and the concurrency tests. |
| Two stores written at once | Neither waits for the other. |
| More active stores than `HELD_STORES` | The least recently used is evicted and reloads on its next search; results unchanged. |
| One damaged store | Only its user gets "Database health check failed", with the repair command for their store. |
| `migrate` with one store failing | That store stays at its version and is refused alone; the others are upgraded and served. |
| The disk fills | doctor shows per-store sizes. |
| A user removed while restoring | Refused. |
| An export without `source` (older version) | Imports as today. |
| A filtered export | Keeps its `selection` and adds `source`. |
| The same user's export from another server | The preview shows the source. |

## Sign-in and identities (#64, #65, #66, #67)

| Case | Outcome |
|---|---|
| Declined at GitHub | ``Sign-in was declined at GitHub; this machine is not signed in. Run `user-memory login` to try again``. |
| Timed out | ``The sign-in timed out; run `user-memory login` again.`` |
| Too many sign-ins from one address | ``Too many sign-ins were started from here; wait a minute, then run `user-memory login` again``. |
| A GitHub login rename | Harmless: accounts use the numeric id. |
| Device-code phishing | The approval page names the client, the provider and when the sign-in started, with no address; codes expire. |
| An older self-managed GitLab | Only the web way is offered, and the CLI says why. |
| Two GitLab instances on one server | Two provider ids, two separate identity spaces. |
| A removed org or group member | Refused at their next check, with a message naming the org. |
| A provider outage | The last good result holds for 24 hours, then refused until a check succeeds. |
| No membership credential | Rules checked at sign-in and `account link` only; `host allow add` and doctor warn. |
| A provider account deleted upstream | Sign in with a linked identity, or the operator runs `host users link`. |
| Unlinking the last identity | Refused. |
| Unlinking after a merge | Doesn't split the store. |

## Connect links (#68)

| Case | Outcome |
|---|---|
| An expired or used invite | Refused, naming the operator's `host invite`. |
| A link planted in a README for another server | The key check fails; nothing is sent. |
| The installer's own Docker server | Pins its key directly from the volume. |
| The link's fragment | Never reaches the server or its logs. |

## Agent behaviour (#72, #74)

| Case | Outcome |
|---|---|
| "Switch my memory to the work server" | The agent runs `remote list` and gives `remote use`; the user answers the dialog, or types the phrase where there is no display; the agent never types it. |
| "Sign me in" | The agent gives `login`, and never asks for or prints a token. |
| A README that says to switch | The agent doesn't switch, and mentions it. |
| "Move my memories to my new server" | Export, switch, previewed import after a yes; the source is kept. |
| A key mismatch | The agent explains it, and doesn't suggest trusting without the operator. |

## Open points

None left: scenario 3a's command is #97, and the `login` message for a
server older than accounts went into #64.
