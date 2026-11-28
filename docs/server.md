# Running the server

The memory server holds your memories and the access tokens. Agents reach it
through the `user-memory` client on each machine. You can run it in three
ways:

| Way | For | Clients connect to |
|---|---|---|
| Docker, from the installer | One machine. | `http://127.0.0.1:8765` |
| Native, from the installer | One machine, without Docker. | Nothing: each agent session runs its own server. |
| Docker, shared behind HTTPS | All your machines share one memory. | `https://<your host>` |

- [Docker on this machine](#docker-on-this-machine)
- [Native on this machine](#native-on-this-machine)
- [A shared server behind HTTPS](#a-shared-server-behind-https)
- [Access tokens](#access-tokens)
- [Semantic search](#semantic-search)
- [Upgrade](#upgrade)
- [Back up and restore](#back-up-and-restore)
- [Check and repair](#check-and-repair)

## Docker on this machine

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --local docker
```

The installer builds a server image from the release, starts it on
`127.0.0.1:8765`, gives the access token to the client and connects your
agents. The server restarts with Docker (`restart: unless-stopped`); turn on
Docker Desktop's start at login so memory is there after a reboot.

Its files are in `~/.local/share/user-memory/docker/`. To manage it:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml stop
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml start
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml logs memory
```

The memories live in the Docker volume `user-memory-data`: the store
(`/data/memories.sqlite3`, with its write-ahead log files `-wal` and `-shm`
beside it while it is open, and the writers' empty lock file), the access
token, the upgrade backups and the automatic backups in `backups/`. Stopping,
restarting and rebuilding keep it. `docker compose … down --volumes` deletes
it, with every memory and the token. Keep Docker's volumes on a local disk:
the store's write-ahead log cannot be shared over a network filesystem (NFS,
SMB).

The container runs as an unprivileged user (UID and GID 10001), with a
read-only root filesystem and no capabilities, and stops cleanly on Docker's
stop signal.

The installer refuses to start a second server on the same volume, and
refuses a port that is already in use (pass `--port N` for another one).

On Windows the same server runs in Docker Desktop, which must be running:
give the [Windows installer](install.md#install-on-windows) `-Local docker`.
Its files are in `%LOCALAPPDATA%\user-memory\docker\`; in PowerShell, manage
it with `docker compose -f "$env:LOCALAPPDATA\user-memory\docker\compose.yaml"`
and `stop`, `start` or `logs memory`. On an ARM64 PC, Docker Desktop runs the
ARM64 server build. For semantic search the installer offers Ollama on this
PC; `-InstallOllama` installs it with `winget`.

## Native on this machine

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --local native
```

There is no server to run. Each agent session starts its own, on the store
`~/.local/share/user-memory/memories.sqlite3`. Keep that folder on a local
disk; `user-memory doctor` says why:

```output
Journal mode: WAL; keep the store on a local disk, since a network filesystem (NFS, SMB) cannot share its write-ahead log between processes
```

## A shared server behind HTTPS

To use one memory on all your machines, run the installer's Docker server on
one host and put an HTTPS proxy, such as nginx, in front of it. Machines
other than the host connect only through the proxy.

1. On the host, set up the Docker server with the proxy's host name:

   ```bash
   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --local docker --allowed-host memory.example.com
   ```

   On a host that runs the server and no agent app (no Claude Code or
   Codex), use `--server-only` instead of `--local docker`:

   ```bash
   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --server-only --allowed-host memory.example.com
   ```

   It sets up the same Docker server and installs `user-memory` for
   `doctor`, `backup` and uninstalling, but connects no agent app: it writes
   no registration, hooks, skill, instruction files or git ignore lines.
   Running the installer again keeps it so; `--client` or `--url` makes the
   host a client too. `user-memory doctor` there checks only the server:
   that it answers with the host's token, then the store inside the
   container.

   The server then accepts requests for that name, besides `localhost` and
   `127.0.0.1`, so the host's own agents keep connecting directly.
   `--allowed-host` takes a bare host name, without `https://` or a path,
   and may be given more than once.

   A proxy on the host itself forwards to `http://127.0.0.1:8765`. A proxy on
   another machine needs the server on an address it can reach: add
   `--bind 10.0.0.5` (a private or tunnel IPv4 address), and the server is
   published there as well as on `127.0.0.1`. The installer refuses `0.0.0.0`
   and `::`, since the server speaks plain HTTP:

   ```output
   user-memory install: --bind 0.0.0.0 would serve plain HTTP on every network; keep 127.0.0.1 for an HTTPS proxy on this machine, or bind a private or tunnel address that only the proxy reaches
   ```

   Running the installer again keeps both settings; giving an option again
   replaces it. At the end, the installer prints the address other machines
   use and the command to run on each:

   ```output
   Other machines reach it through your HTTPS proxy, which forwards to http://127.0.0.1:8765, at:
   …
   Give each of them its own token: run user-memory host token add NAME --out FILE
   here, copy FILE to that machine over a private channel, then run there:
   ```

2. Configure the proxy. The parts that matter for nginx:

   ```nginx
   client_max_body_size 2m;

   location = /mcp {
       proxy_pass http://127.0.0.1:8765;
       proxy_set_header Host $host;                 # must be an --allowed-host
       proxy_set_header X-Forwarded-Proto $scheme;
       proxy_http_version 1.1;
       proxy_set_header Connection "";
       proxy_buffering off;                         # responses stream
       proxy_request_buffering off;
       proxy_read_timeout 300s;
       proxy_send_timeout 300s;
   }

   location ~ ^/(export|import|delivery|titles)$ {  # export, import, delivery index, titles
       client_max_body_size 129m;               # a 64 MiB file and titles map
       proxy_pass http://127.0.0.1:8765;
       proxy_set_header Host $host;
       proxy_set_header X-Forwarded-Proto $scheme;
       proxy_http_version 1.1;
       proxy_set_header Connection "";
       proxy_buffering off;
       proxy_request_buffering off;
       proxy_read_timeout 300s;
       proxy_send_timeout 300s;
   }

   location ~ ^/(use|tags|auth/token)$ {      # session hooks, a token's scope
       proxy_pass http://127.0.0.1:8765;        # bodies are at most 256 KiB
       proxy_set_header Host $host;
       proxy_set_header X-Forwarded-Proto $scheme;
       proxy_http_version 1.1;
       proxy_set_header Connection "";
       proxy_read_timeout 30s;
       proxy_send_timeout 30s;
   }

   location = / {                               # the health line
       proxy_pass http://127.0.0.1:8765;
       proxy_set_header Host $host;
       proxy_set_header X-Forwarded-Proto $scheme;
   }
   ```

   Keep the `Authorization` header intact: the server checks the token
   itself. `/use` and `/tags` carry what the session hooks send: what a
   session used, and the tag conditions to check and those a checkout
   ended. `/auth/token` answers or revokes the calling token's own scope.
   `GET /` answers one line without a token, for health checks. The server
   answers 404 on any other path, so nothing else needs forwarding. Avoid a
   CDN in front of the server: CDNs often strip `Authorization`, cache or
   restrict `POST`, and cut streamed responses short. Point the host name
   straight at the proxy (on Cloudflare: a DNS-only record).

   `/titles` sends a client's connector the title, source and type of
   each memory its sessions' projects can be pushed, so a push names each
   memory by its title and marks an agent's note: whole once, under 2 MB
   at 10,000 memories, then only what changed. What changed lists the ids
   of memories changed anywhere in the store that the scope doesn't hold,
   another project's included but never their titles, as `/delivery`'s
   changes do: a token reaches every project anyway. A proxy that doesn't
   let `/titles` through leaves pushes naming ids alone, and
   `user-memory doctor` on the client says so.

   `/delivery` sends each client the delivery index of its sessions'
   projects, 2.7 MB at 10,000 memories, when it has none. The server keeps
   the last one it built for each of up to 32 sets of projects in memory,
   about 90 MB at most at 10,000 memories, so many clients starting at once
   cost one build.

3. Check it from another machine. The first command prints one line; the
   second prints `401`, because it sends no token:

   ```bash
   curl -fsS https://memory.example.com/
   ```

   ```bash
   curl -s -o /dev/null -w '%{http_code}\n' -X POST https://memory.example.com/mcp
   ```

4. Give every other machine its own token
   ([access tokens](#access-tokens)): on the host, run
   `user-memory host token add NAME --out FILE`, copy the file to that
   machine over a private channel, and install the client there with it:

   ```bash
   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --url https://memory.example.com --token-file /path/to/token
   ```

   Then run `user-memory doctor` there.

## Access tokens

Anyone with a token that can write can read and change your memories. Treat
each token like a password: an agent names a token file by its path and
never opens it.

Each machine gets its own token from the server, so you can see when it was
last used and revoke it alone. The server's shared token file, which the
installer gives the machine it runs on, keeps working until you
[retire it](#retire-the-shared-token-file).

### Give a machine its own token

On the machine that runs the server:

```bash
user-memory host token add laptop --out laptop.token
```

This writes a new token for `laptop` to `laptop.token`, readable only by
you; `add` never replaces a file. Copy the file to that machine over a
private channel, such as `scp` or a password manager, and install there with
it:

```bash
curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --url https://memory.example.com --token-file /path/to/laptop.token
```

| Option | Effect |
|---|---|
| `--read-only` | The token reads and searches memories, and the session hooks work, but nothing changes a memory: the server refuses `record`, `update`, `forget`, `import`, `review` with `keep`, and `export` with `overwrite`. |
| `--expires DAYS` | The token stops working after DAYS days. |

Every token also stops working after 90 days unused. Without `--out`, `add`
shows the token only on a terminal. On the machine of the installer's
Docker server, `host token` runs in its container; for a native server,
give `--db` with the server's store.

### See and revoke tokens

```bash
user-memory host token list
```

```bash
user-memory host token revoke laptop
```

`list` shows each token's name, scope, creation, last use (to the hour) and
expiry. The server refuses a revoked or expired token from its next request,
without a restart, and that machine tells its user why and what to do:
ask the server's operator for a new token file and run the installer with
it. A machine that leaves the server can revoke its own token with
`DELETE /auth/token`. `GET /auth/token` answers the calling token's scope,
`{"scope":"read"}` or `{"scope":"write"}` (the shared token file writes);
session start asks it to say who can remove a new rule.

### Retire the shared token file

The shared token file (`/data/access-token` in Docker) lets in anyone who
holds it until it is retired. Once every machine has its own token:

```bash
user-memory host token retire-shared
```

On the installer's Docker server, this first gives this machine's client its
own token, `docker-host`; if the server's healthcheck still reads the shared
file, run the installer again first, as the command says. For a native
server, give the file and the store:
`user-memory host token retire-shared --token-file /path/to/access-token --db /path/to/memories.sqlite3`.
It refuses while no client token can write, so the shared file is never the
last way in.

The server refuses the shared token from its next request, and the file gets
a new value that no machine holds. If the command stops partway, run it
again; it finishes from where it stopped. Until it has finished, a server
rolled back to a release without client tokens would still accept the old
shared token.

### Where tokens are kept

The server keeps each token's SHA-256, never the token, in
`accounts.sqlite3` beside the store, which the first `add` creates. Neither
automatic backups nor exports include it: after restoring a server
elsewhere, issue its machines new tokens. While the registry cannot be
read, the server answers every request that needs a token with 503 and a
sentence that names no file; its log says why and what to do
([troubleshooting](troubleshooting.md)).

While the shared token file is not retired, you can change its token:
delete the server's token file (`/data/access-token` in Docker), restart the
server so it creates a new one, then run the installer again on every
machine that uses it. Once it is retired, its token is refused whatever it
holds; give each machine its own token instead.

## Semantic search

Without an embeddings model, search matches keywords only. With one, it also
finds memories phrased differently from the question, and catches more
duplicates. Any OpenAI-compatible embeddings endpoint works. The
recommended one is `qwen3-embedding:4b` (about 2.5 GB) served by
[Ollama](https://ollama.com) on the same machine, which user-memory is
calibrated for in advance; on a Mac, Ollama on the host uses the GPU, which a
container cannot.

The installer asks which to use ([semantic search](install.md#semantic-search)).
To change it later on a server the installer set up on this machine, Docker
or native:

```bash
user-memory embedding set --url http://localhost:11434 --model qwen3-embedding:4b
```

`--url` takes a host (`/v1/embeddings` is added), a base URL such as
`https://api.openai.com/v1` (`/embeddings` is added), or the full endpoint;
`--key-file FILE` names a file holding an API key, and `--dimensions N` asks
for shorter vectors. `embedding set` first checks that the endpoint answers,
and changes nothing if it does not. It then calibrates the model (see below),
saves the choice where the memories live (for the Docker server, it
recreates the container with it) and embeds every memory again, since
vectors from two models cannot be compared. Running the installer again with
`--embed-model`, `--embed-url` or `--no-embed` does the same
([install without questions](install.md#install-without-questions)).

The installer's Docker server reads the setting from
`~/.local/share/user-memory/docker/embedding.env`, and an API key from its
volume; a native store keeps it beside the binary. The Docker server reaches
Ollama on this machine as `host.docker.internal`. Docker Desktop on a Mac or
PC forwards that to Ollama's default `127.0.0.1:11434`. With Docker Engine on
Linux, Ollama must also listen on the Docker bridge address, for example
with `OLLAMA_HOST=172.17.0.1:11434`. Docker Desktop on Linux runs containers
in a virtual machine, as on a Mac; if the probe line of
`user-memory embedding show` says it failed there, set Ollama to listen on
the bridge address the same way.

`user-memory embedding show` prints the endpoint, the model, its vector
length and how duplicates are detected. `user-memory embedding off` returns
to keyword search and drops the stored vectors. On the machine that runs the
installer's Docker server, run them all there, not in the container: `show`
prints the setting from `embedding.env`, naming `host.docker.internal` as
this machine and an API key only as ``API key: in the server's volume``, then
the container's own probe and calibration. When the container isn't
running, it prints the command for the rest instead, as
``Probe and stored vectors: docker compose -f … exec memory user-memory embedding show``.
`embedding sync` runs in the container the same way.

A model other than `qwen3-embedding:4b` is calibrated automatically: user-memory
embeds example memory pairs that ship with it (restatements, exceptions,
related and unrelated rules), sets the similarity at which it flags a
possible duplicate from those scores, and keeps the result with the store.
When a model cannot tell the examples apart, `embedding set` says so; search
still uses the model, but duplicates are caught by keywords only, and
`doctor` notes it.

A server keeps every memory's current vector in memory after its first
semantic search, so later searches compare the question with all of them at
once and read only the vectors that changed since. That takes the vectors'
own size: about 4 KB per memory at 1,024 dimensions and 10 KB at
`qwen3-embedding:4b`'s default 2,560, so 100 MB or 250 MB for 25,000
memories. A command-line search reads them all each time instead. Besides
the vectors, a server's memory stays about the same however much it
searches: each search reuses the working space of the one before, about
20 MB at 100,000 memories.

`user-memory embedding show` and `user-memory doctor` count your own vectors
where the store is, and on the machine that runs the installer's Docker
server:
``Stored vectors: 4,120 (4,080 current), 16.9 MB; a server holds the current ones in memory``.
They count the vectors of the model and vector length set now, the ones a
server loads. A memory changed since its vector was made is not current: its
next search or `user-memory reindex` makes a new one.

With semantic search on, memory text is sent to the embeddings endpoint. Keep
it on your own machine or a server you control. For an endpoint that isn't
on this machine, `embedding set`, `embedding show` and `doctor` name its
host.

## Upgrade

Run the installer again on the server's machine, then on every client.
[Upgrade a shared server](upgrade.md#upgrade-a-shared-server) says what it
does and how to undo it.

While the server restarts, a client's call gets
``The memory server at … did not answer; the call may not have completed``
and the next call reconnects ([troubleshooting](troubleshooting.md#from-doctor)).
Clients don't restart for a server restart or upgrade, only when their own
client is upgraded.

A client upgraded before its server still works with it, except for what
the server is too old for. A server ignores an option it doesn't know, so
the client asks only for what the server's version has: memory history
needs 0.10.0 on the server. Asked of an older one, `user-memory history`
and `read --history` stop, and an agent's `read` with `history` is
answered with
``The memory server at … runs user-memory …, which keeps no history (history needs … or newer). Upgrade the server on its machine.``
Everything else goes on as before. The agent tells you the server is too
old rather than reading the memory as one that never changed. Once the
server is upgraded, the next history read reaches it.

A session that starts while the server is down, or while it refuses the
client's token, still gets its memory tools. If the server hasn't answered
within 3 seconds, the client answers the agent app's handshake and tool
list itself, with its own version's. Claude Code has the tools from the
session's first turn; Codex, which starts its first turn before its tools
are in, from the next one, unless the server refused the connection at
once. Each call then gets the reply for the outage or the refused token.
Once the server answers, the next call reaches it, and the session
recovers by itself. A server that is only slow keeps the 30 seconds the
apps give a server to start: until then, a call waits for its handshake. When the server's tools
differ from the client's own, the app is told. Session start's index is
not loaded again; the agent calls `index` itself. A server that no longer
speaks the protocol version the client agreed with the app still gets the
session's requests as they are, as after any reconnect, and answers any it
can't read with an error.

## Back up and restore

The server backs its store up once a day, as a native store does
([automatic backups](backup.md#automatic-backups)): the first change once the
newest backup is a day old copies the store into `/data/backups/` in the
volume, and the newest 7 are kept. The server also checks every hour, and as
it stops, so changes followed by an hour without any are copied too,
replacing that day's backup. Doctor on the server reports the newest.

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory backup list
```

To restore one, stop the server, delete the log files and the marker of the
store's changes, and copy the backup over the store inside the volume, then
start it again:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml stop memory
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint rm memory -f /data/memories.sqlite3-wal /data/memories.sqlite3-shm /data/memories.sqlite3.changed
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint cp memory /data/backups/memories-<date>.sqlite3 /data/memories.sqlite3
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml start memory
```

These backups sit on the same disk as the store. For a copy elsewhere, export
regularly, and keep the access token with your exports: a new token means
reinstalling every client.

From any machine connected to the server, `user-memory export FILE` and
`user-memory import FILE` work as with a local install: the file is written
or read on that machine. On the server's machine you can also go through
Docker directly. Export through stdout to a file outside the container:

```bash
(umask 077; docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec -T memory user-memory export - > memories-$(date -u +%Y%m%dT%H%M%SZ).json)
```

Restore through stdin. The first command only shows what would change; the
second writes it. A third, optional, embeds the new memories at once; search
embeds them as it goes otherwise:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec -T memory user-memory import - < memories.json
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec -T memory user-memory import - --apply < memories.json
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory reindex
```

[Back up and restore](backup.md) covers exports, a native store and moving
one to a server.

## Check and repair

```bash
user-memory doctor
```

On the Docker server itself:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory doctor
```

Nothing is installed inside the container, and its image names the store in
`USER_MEMORY_DB`, so doctor says ``Nothing is installed here; checking the store
at /data/memories.sqlite3`` and checks the store, its index, journal mode,
semantic search settings and automatic backups.

| You see | Do this |
|---|---|
| Clients show ``User memory unavailable`` | Check that the server runs (`docker compose -f ~/.local/share/user-memory/docker/compose.yaml ps`) and answers: `curl -fsS http://127.0.0.1:8765/`, or your HTTPS address. |
| `401` for every client | The clients' token no longer matches the server's. Copy the token again and run the installer on each machine. |
| The server refuses to start: ``run the installer again (or ./setup, or `user-memory migrate`) to upgrade it`` | The store needs an [upgrade](#upgrade). |
| ``Database health check failed`` | The store is damaged. Run `user-memory repair` where the store lives (Docker: `docker compose … exec memory user-memory repair`). It backs the store up and rebuilds its indexes. |
| Search finds only memories that share your words | Semantic search is off or its endpoint is down. Run `user-memory embedding show` and `doctor`, and check that Ollama runs and has the model. |
