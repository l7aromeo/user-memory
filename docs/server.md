# Running the server

The memory server holds your memories and the access token. Agents reach it
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
- [The access token](#the-access-token)
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
   On each of them, save this machine's access token (from <token file>) in a
   file with mode 0600, then run:
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

   location ~ ^/(export|import)$ {              # export and import from clients
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
   ```

   Keep the `Authorization` header intact: the server checks the token
   itself. `GET /` answers one line without a token, for health checks.
   Everything else may return 404. Avoid a CDN in front of the server: CDNs
   often strip `Authorization`, cache or restrict `POST`, and cut streamed
   responses short. Point the host name straight at the proxy (on
   Cloudflare: a DNS-only record).

3. Check it from another machine. The first command prints one line; the
   second prints `401`, because it sends no token:

   ```bash
   curl -fsS https://memory.example.com/
   ```

   ```bash
   curl -s -o /dev/null -w '%{http_code}\n' -X POST https://memory.example.com/mcp
   ```

4. On every other machine, save a copy of the host's access token
   ([the access token](#the-access-token)) in a file only you can read, and
   install the client with it:

   ```bash
   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- --url https://memory.example.com --token-file /path/to/token
   ```

   Then run `user-memory doctor` there.

## The access token

Anyone with the token can read and change your memories. Treat it like a
password.

- The server creates it on first start and never prints it.
- Each client keeps its copy in `~/.config/user-memory/client-token`,
  readable only by you.
- The installer reads it from `--token-file`, or asks for it with hidden
  input.

To copy the token of the installer's Docker server into a file without
showing it:

```bash
mkdir -p -m 700 ~/.config/user-memory
(umask 077; docker compose -f ~/.local/share/user-memory/docker/compose.yaml cp memory:/data/access-token ~/.config/user-memory/client-token)
```

For a server on another host, run that `docker compose cp` there, or copy
the file over SSH without showing it:
`(umask 077; ssh <host> 'docker exec <container> cat /data/access-token' > ~/.config/user-memory/client-token)`.

To change the token: delete the server's token file (`/data/access-token` in
Docker), restart the server so it creates a new one, then run the installer
again on every machine with the new token.

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
Ollama on this machine as `host.docker.internal`: Docker Desktop on a Mac or
PC forwards that to Ollama's default `127.0.0.1:11434`, but on Linux Ollama
must also listen on the Docker bridge address, for example with
`OLLAMA_HOST=172.17.0.1:11434`.

`user-memory embedding show` prints the endpoint, the model, its vector
length and how duplicates are detected. `user-memory embedding off` returns
to keyword search and drops the stored vectors.

A model other than `qwen3-embedding:4b` is calibrated automatically: user-memory
embeds example memory pairs that ship with it (restatements, exceptions,
related and unrelated rules), sets the similarity at which it flags a
possible duplicate from those scores, and keeps the result with the store.
When a model cannot tell the examples apart, `embedding set` says so; search
still uses the model, but duplicates are caught by keywords only, and
`doctor` notes it.

With semantic search on, memory text is sent to the embeddings endpoint. Keep
it on your own machine or a server you control.

## Upgrade

For the installer's Docker or native server, run the installer again. It
backs the store up before it changes the store's format
([upgrade](install.md#upgrade)). For the Docker server, the backup stays in
the volume, at `/data/memories.sqlite3.v<old version>-<time>.bak`.

The server never upgrades its store itself: until the installer has upgraded
it, the server refuses it with
``run the installer again (or ./setup, or `user-memory migrate`) to upgrade it``.
An upgrade from 0.3 needs a title for each memory
([titles](install.md#upgrade-from-03-titles)).

To undo an upgrade, restore the backup and run the previous version's
installer with its tag: [roll back an upgrade](install.md#roll-back-an-upgrade)
gives the steps for the Docker server and a native store. Memories recorded
since the upgrade are lost unless you exported them first.

## Back up and restore

The server backs its store up once a day, as a native store does
([automatic backups](cli.md#automatic-backups)): the first change once the
newest backup is a day old copies the store into `/data/backups/` in the
volume, and the newest 7 are kept. Doctor on the server reports the newest.

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory backup list
```

To restore one, stop the server, delete the log files and copy the backup
over the store inside the volume, then start it again:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml stop memory
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml run --rm --entrypoint rm memory -f /data/memories.sqlite3-wal /data/memories.sqlite3-shm
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
second writes it; the third rebuilds the search index:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec -T memory user-memory import - < memories.json
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec -T memory user-memory import - --apply < memories.json
```

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory reindex
```

With a native store, run `user-memory export memories.json` and
`user-memory import memories.json` directly.

To move memories from a native store to a server: export on the old machine,
import on the server, then run the installer on the old machine with the
server's address.

Restoring an older backup brings back memories forgotten since. Backups do
not include conversation links.

## Check and repair

```bash
user-memory doctor
```

On the Docker server itself:

```bash
docker compose -f ~/.local/share/user-memory/docker/compose.yaml exec memory user-memory doctor
```

| You see | Do this |
|---|---|
| Clients show ``User memory unavailable`` | Check that the server runs (`docker compose -f ~/.local/share/user-memory/docker/compose.yaml ps`) and answers: `curl -fsS http://127.0.0.1:8765/`, or your HTTPS address. |
| `401` for every client | The clients' token no longer matches the server's. Copy the token again and run the installer on each machine. |
| The server refuses to start: ``run the installer again (or ./setup, or `user-memory migrate`) to upgrade it`` | The store needs an [upgrade](#upgrade). |
| ``Database health check failed`` | The store is damaged. Run `user-memory repair` where the store lives (Docker: `docker compose … exec memory user-memory repair`). It backs the store up and rebuilds its indexes. |
| Search finds only exact words | Semantic search is off or its endpoint is down. Run `user-memory embedding show` and `doctor`, and check that Ollama runs and has the model. |
