# Not planned for now

The features below were considered and set aside, each for the reason
beside it. These are current decisions, not permanent ones: any of them
may change when its reason no longer holds.

| Idea | Why not, for now |
|---|---|
| An encrypted store (SQLCipher) | A key on the same disk adds little over disk encryption. It needs a different SQLite build and a new store format, and backup, repair and migration would all be redone. Copies that leave the machine are covered by [#49](https://github.com/l7aromeo/user-memory/issues/49). |
| Tokens limited to one project | Every read path (index, search, read, links, export) would have to enforce the filter. Per-client tokens with a scope ([#48](https://github.com/l7aromeo/user-memory/issues/48)) cover the known needs, and their format leaves room for more scopes. |
| A web inspector | The planned accounts pages ([#64](https://github.com/l7aromeo/user-memory/issues/64), [#68](https://github.com/l7aromeo/user-memory/issues/68)) only sign in and connect a machine, and show no memory text. An inspector renders memory text, which any client or import can set, in a browser: an XSS and CSRF surface over every memory. The CLI (`index`, `read`, `review`, `export`) covers inspection today. |
| Built-in encrypted exports | Exports are already owner-only, and standard tools (age, gpg, encrypted disk images) encrypt a file well. Building it in adds key management and a format change. See [#49](https://github.com/l7aromeo/user-memory/issues/49). |
| A lifecycle visualisation | `read`, `review` and `history` ([#39](https://github.com/l7aromeo/user-memory/issues/39)) show the lifecycle as text. A visual view depends on the web inspector. |
| Duplicate detection by keyword across categories | Shared words would flag unrelated memories. With semantic search on, duplicates are detected across categories; without it, keyword matching stays within a category group. |
| A push channel for session hooks | A running session is told of changes at its next prompt, and its tool hooks fold them in between prompts, so a changed memory is pushed or held at the next step it matches; tools read the live store mid-turn. Push needs a persistent connection from every hook. |
| The source on every index line | Rules already have their own section. The index's character budget is tight, and `read` shows the source. |
| Multilingual evaluation | The stemmer is English-only. An evaluation alone fixes nothing; the work would be stemming for each language plus a benchmark, worth it once memories are written in other languages. See [#3](https://github.com/l7aromeo/user-memory/issues/3). |

Approval through the operating system is planned, so it has no row: a
native dialog asks before a rule is saved
([#53](https://github.com/l7aromeo/user-memory/issues/53)) and before a
sensitive command runs
([#62](https://github.com/l7aromeo/user-memory/issues/62)).
