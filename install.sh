#!/bin/sh
# Installs or upgrades the user-memory client from GitHub Releases and
# connects the Codex and Claude Code installations it finds to a memory
# server: an existing one, or one it sets up on this machine (in Docker or
# as a local store).
#
#   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh
#
# Run it with --help (`… | sh -s -- --help`) for the options. Everything runs
# from main, called on the last line, so a download cut off halfway never
# runs a partial script.

set -eu

# The recommended embedding model and its download size; `embedding set`
# fills in the vector length and query instruction it is calibrated for.
EMBED_MODEL=qwen3-embedding:4b
EMBED_MODEL_SIZE="about 2.5 GB"
OLLAMA_DOWNLOAD=https://ollama.com/download
OLLAMA_INSTALL_SCRIPT=https://ollama.com/install.sh
# Seconds to wait for a newly installed or started Ollama to answer.
OLLAMA_WAIT=30

fail() {
    printf 'user-memory install: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Install or upgrade the user-memory client and connect it to a memory server.

Usage: curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/install.sh | sh -s -- [options]

  --url URL              use an existing server, e.g. https://memory.example.com
                         (/mcp is added when missing)
  --local docker|native  set up a server on this machine instead: a Docker
                         container on 127.0.0.1, or a local store
  --port N               port of the local Docker server (default 8765)
  --allowed-host HOST    for a Docker server shared through an HTTPS proxy:
                         accept requests for HOST too (repeatable; this
                         machine's own clients keep using 127.0.0.1)
  --bind ADDR            also publish the Docker server on this IPv4 address,
                         for an HTTPS proxy on another machine (default:
                         127.0.0.1 only); never 0.0.0.0, as it speaks plain HTTP
  --token-file FILE      use this access-token file as it is
  --version vX.Y.Z       install this release (default: the latest)
  --install-dir DIR      where the binary goes (default ~/.local/bin)
  --client NAME          auto (default), codex, claude or both
  --server-only          for a host with no agent app: run the server in Docker
                         (as --local docker) and connect no client; kept by
                         reruns until --client or --url is given
  --titles FILE          titles map for upgrading a store on this machine to
                         schema version 3: a JSON object mapping each memory
                         id to the title the user approved

Semantic search, for a server set up on this machine (asked on a terminal;
a rerun keeps the earlier choice unless one of these is given):
  --embed-model NAME     use Ollama on this machine with this embedding model
                         (default qwen3-embedding:4b)
  --install-ollama       install Ollama if it is missing (Homebrew on macOS,
                         Ollama's install script with sudo on Linux)
  --pull-model           download the model into Ollama if it is missing
                         (qwen3-embedding:4b is about 2.5 GB)
  --embed-url URL        use another OpenAI-compatible endpoint instead, with
                         --embed-model; /embeddings is added when missing
                         (/v1/embeddings for a bare host)
  --embed-key-file FILE  API key for --embed-url, read from FILE when used
  --embed-dimensions N   ask the model for vectors of length N
  --no-embed             keyword search only

Without --url or --local, the installer reuses what an earlier install set
up, or asks. The server address can also come from USER_MEMORY_URL, and the
access token from USER_MEMORY_TOKEN; tokens are saved to
~/.config/user-memory/client-token (mode 0600) and never printed.
EOF
}

absolute() {
    case $1 in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$PWD" "$1" ;;
    esac
}

# Checks an http(s)://host[:port][/path] address and prints it without
# surrounding space or trailing slashes.
clean_url() {
    url=$(printf '%s' "$1" | tr -d '[:space:]')
    while :; do
        case $url in
            */) url=${url%/} ;;
            *) break ;;
        esac
    done
    printf '%s\n' "$url" |
        grep -Eq '^https?://([A-Za-z0-9.-]+|\[[0-9A-Fa-f:.]+\])(:[0-9]{1,5})?(/[A-Za-z0-9._~/-]*)?$' ||
        fail "not an address: $1 (expected http(s)://host[:port][/path])"
    printf '%s\n' "$url"
}

# The MCP endpoint of a server address: /mcp appended unless the path
# already ends in it.
endpoint_of() {
    url=$(clean_url "$1")
    rest=${url#*://}
    case $rest in
        */*) path=/${rest#*/} ;;
        *) path= ;;
    esac
    case $path in
        */mcp) printf '%s\n' "$url" ;;
        *) printf '%s/mcp\n' "$url" ;;
    esac
}

# Whether URL is plain http to this machine.
loopback_http() {
    rest=${1#http://}
    [ "$rest" != "$1" ] || return 1
    case ${rest%%/*} in
        127.0.0.1 | 127.0.0.1:* | localhost | localhost:* | \[::1\] | \[::1\]:*) return 0 ;;
    esac
    return 1
}

# Fails unless URL is https, or plain http to this machine: tokens and
# binaries never travel unencrypted across a network.
require_secure() {
    case $1 in
        https://*) return 0 ;;
    esac
    loopback_http "$1" ||
        fail "$2 $1 uses plain HTTP; use https:// (plain http is allowed only for this machine)"
}

# The Rust target triple of the client build for this machine.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case $os/$arch in
        Darwin/arm64 | Darwin/aarch64) echo aarch64-apple-darwin ;;
        Darwin/x86_64)
            # A shell under Rosetta reports x86_64 on Apple silicon.
            if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
                echo aarch64-apple-darwin
            else
                echo x86_64-apple-darwin
            fi
            ;;
        Linux/x86_64 | Linux/amd64) echo x86_64-unknown-linux-gnu ;;
        Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-gnu ;;
        *) fail "there is no client build for $os $arch" ;;
    esac
}

sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        fail "neither shasum nor sha256sum is available to verify the download"
    fi
}

# GitHub's release CDN now and then cuts a transfer off midway (curl error
# 92, an HTTP/2 stream reset) and an immediate retry succeeds. Three retries
# ride out such a blip; a download that keeps failing still fails in seconds.
FETCH_RETRIES=3
# Seconds between attempts, enough for a reset connection to be replaced.
FETCH_RETRY_DELAY=1

# Downloads URL to FILE once; over https, redirects may not drop to plain http.
fetch_once() {
    case $1 in
        https://*) curl -fsSL --proto '=https' "$1" -o "$2" ;;
        *) curl -fsSL "$1" -o "$2" ;;
    esac
}

# Downloads URL to FILE, retrying any failure. The loop rather than curl's
# --retry-all-errors keeps curl older than 7.71 working.
fetch() {
    retries=0
    until fetch_once "$1" "$2"; do
        [ "$retries" -lt "$FETCH_RETRIES" ] || return 1
        retries=$((retries + 1))
        printf 'Retrying the download (%s of %s)\n' "$retries" "$FETCH_RETRIES" >&2
        sleep "$FETCH_RETRY_DELAY"
    done
}

# Downloads the release's build for TARGET to FILE and checks its SHA-256.
download() {
    fetch "$ASSETS/user-memory-$1" "$2" || fail "cannot download $ASSETS/user-memory-$1"
    fetch "$ASSETS/user-memory-$1.sha256" "$2.sha256" ||
        fail "cannot download $ASSETS/user-memory-$1.sha256"
    expected=$(awk '{print $1; exit}' "$2.sha256")
    case $expected in
        *[!0-9a-f]* | '') fail "the release's checksum file is malformed; nothing was installed" ;;
    esac
    [ ${#expected} -eq 64 ] || fail "the release's checksum file is malformed; nothing was installed"
    [ "$(sha256_of "$2")" = "$expected" ] ||
        fail "the download does not match its checksum; nothing was installed"
    chmod 755 "$2"
}

# `true`, not `:`: a failed redirection on the special builtin `:` ends the
# whole script under dash, Debian's and Ubuntu's /bin/sh.
have_tty() {
    { true </dev/tty; } 2>/dev/null
}

# Asks QUESTION on the terminal; DEFAULT (y or n) answers an empty line.
ask() {
    case $2 in
        y) choices='[Y/n]' ;;
        *) choices='[y/N]' ;;
    esac
    while :; do
        printf '%s %s ' "$1" "$choices" >/dev/tty
        IFS= read -r answer </dev/tty || answer=
        case $answer in
            '') [ "$2" = y ] && return 0 || return 1 ;;
            y | Y | yes | Yes | YES) return 0 ;;
            n | N | no | No | NO) return 1 ;;
        esac
    done
}

# Saves TOKEN to TOKEN_FILE: a private directory, mode 0600, replaced in one
# rename. printf is a shell builtin, so the token never appears in a process
# list.
save_token() {
    token=$1
    [ ${#token} -ge 32 ] || fail "that is not an access token (it has fewer than 32 characters)"
    private_dir "$(dirname "$TOKEN_FILE")"
    (
        umask 077
        printf '%s\n' "$token" >"$TOKEN_FILE.$$"
        mv -f "$TOKEN_FILE.$$" "$TOKEN_FILE"
    ) || fail "cannot save the access token as $TOKEN_FILE"
}

private_dir() {
    (umask 077 && mkdir -p "$1") || fail "cannot create $1"
    chmod 700 "$1"
}

# Reads the token from the terminal without echoing it.
prompt_token() {
    have_tty || fail "no access token: set USER_MEMORY_TOKEN or pass --token-file"
    # Echo goes off before the prompt appears, so nothing typed early shows.
    stty -echo </dev/tty
    trap 'stty echo </dev/tty; rm -rf "$tmp"; exit 1' INT TERM
    printf 'Access token for %s (input hidden): ' "$URL" >/dev/tty
    IFS= read -r token </dev/tty || token=
    stty echo </dev/tty
    trap 'rm -rf "$tmp"; exit 1' INT TERM
    printf '\n' >/dev/tty
    save_token "$token"
}

docker_ready() {
    command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1
}

# Whether Ollama answers at OLLAMA.
ollama_running() {
    curl -fsS --max-time 3 "$OLLAMA/api/tags" >/dev/null 2>&1
}

# Whether the Ollama app is installed, in USER_MEMORY_APP_DIRS (a list like
# PATH; empty searches nowhere) or else /Applications and ~/Applications.
ollama_app() {
    found=1
    old_ifs=$IFS
    IFS=:
    for dir in ${USER_MEMORY_APP_DIRS-/Applications:$HOME/Applications}; do
        [ -z "$dir" ] || [ ! -d "$dir/Ollama.app" ] || found=0
    done
    IFS=$old_ifs
    return "$found"
}

# Waits up to OLLAMA_WAIT seconds for Ollama to answer.
wait_for_ollama() {
    waited=0
    until ollama_running; do
        [ "$waited" -lt "$OLLAMA_WAIT" ] || return 1
        waited=$((waited + 1))
        sleep 1
    done
}

# The models Ollama has pulled that make embeddings, one per line: those
# whose capabilities name embedding, or, from an Ollama too old to report
# capabilities, whose name says embed.
ollama_embedding_models() {
    curl -fsS --max-time 5 "$OLLAMA/api/tags" 2>/dev/null | tr ',' '\n' | tr '{' '\n' | tr '}' '\n' |
        sed -n 's/^ *"name": *"\([^"]*\)" *$/\1/p' | while IFS= read -r name; do
        shown=$(curl -fsS --max-time 5 -d "{\"model\":\"$name\"}" "$OLLAMA/api/show" 2>/dev/null | tr -d ' \n')
        case $shown in
            *'"capabilities":['*) case ${shown#*'"capabilities":['} in
                '"embedding"'* | *',"embedding"'*) printf '%s\n' "$name" ;;
            esac ;;
            *) case $name in *embed*) printf '%s\n' "$name" ;; esac ;;
        esac
    done
}

# Installs Ollama, only once the user has said yes: Homebrew on macOS, or
# the download page and a wait without it; Ollama's own script on Linux.
install_ollama_now() {
    case $(uname -s) in
        Darwin)
            if command -v brew >/dev/null 2>&1; then
                echo "Installing Ollama with Homebrew (brew install ollama), then starting it (brew services start ollama)"
                brew install ollama </dev/null || fail "brew install ollama failed (above)"
                brew services start ollama </dev/null || fail "brew services start ollama failed (above)"
            else
                have_tty || fail "Homebrew is not installed, so Ollama cannot be installed here; install it from $OLLAMA_DOWNLOAD, then run the installer again"
                printf 'Download Ollama from %s, install it and open it.\nPress Enter once Ollama is running: ' "$OLLAMA_DOWNLOAD" >/dev/tty
                IFS= read -r _ </dev/tty || :
            fi
            ;;
        Linux)
            echo "Ollama's install script ($OLLAMA_INSTALL_SCRIPT) installs it for the whole system and asks for your password (sudo)."
            fetch "$OLLAMA_INSTALL_SCRIPT" "$tmp/ollama-install.sh" || fail "cannot download $OLLAMA_INSTALL_SCRIPT"
            sh "$tmp/ollama-install.sh" </dev/null || fail "Ollama's install script failed (above)"
            ;;
        *) fail "Ollama cannot be installed automatically on $(uname -s); install it from $OLLAMA_DOWNLOAD" ;;
    esac
}

# Downloads MODEL into Ollama, with the Ollama command's progress when it is
# there.
pull_model() {
    if [ "$1" = "$EMBED_MODEL" ]; then
        printf 'Downloading %s (%s)\n' "$1" "$EMBED_MODEL_SIZE"
    else
        printf 'Downloading %s\n' "$1"
    fi
    if command -v ollama >/dev/null 2>&1; then
        OLLAMA_HOST=$OLLAMA ollama pull "$1" </dev/null || fail "ollama pull $1 failed (above)"
    else
        curl -fsS -d "{\"model\":\"$1\",\"stream\":false}" "$OLLAMA/api/pull" >/dev/null ||
            fail "Ollama could not download $1"
    fi
}

# Asks for a number from 1 to COUNT on the terminal; DEFAULT answers an
# empty line. Prints the number.
choose() {
    while :; do
        printf '%s [%s]: ' "$1" "$3" >/dev/tty
        IFS= read -r answer </dev/tty || answer=
        answer=${answer:-$3}
        case $answer in
            '' | *[!0-9]*) ;;
            *) if [ "$answer" -ge 1 ] && [ "$answer" -le "$2" ]; then
                printf '%s\n' "$answer"
                return 0
            fi ;;
        esac
    done
}

# Sets EMBED_URL and EMBED_NAME to Ollama on this machine and a model it
# has, installing Ollama or downloading the model only with the user's yes
# (a terminal answer, or --install-ollama and --pull-model). Returns 1 when
# the user declines, which leaves semantic search off.
use_ollama() {
    if ! ollama_running; then
        if command -v ollama >/dev/null 2>&1 || ollama_app; then
            if [ "$1" = ask ]; then
                printf "Ollama is installed but not running at %s. Start it (open the Ollama app, or run 'ollama serve'), then press Enter: " "$OLLAMA" >/dev/tty
                IFS= read -r _ </dev/tty || :
            else
                fail "Ollama is installed but not running at $OLLAMA; start it, then run the installer again"
            fi
        elif [ "$install_ollama" = yes ]; then
            install_ollama_now
        elif [ "$1" = ask ]; then
            ask "Ollama isn't installed. Install it now?" n || return 1
            install_ollama_now
        else
            fail "Ollama isn't installed; pass --install-ollama to install it, or install it from $OLLAMA_DOWNLOAD"
        fi
        wait_for_ollama || fail "Ollama does not answer at $OLLAMA; start it, then run the installer again"
    fi
    models=$(ollama_embedding_models)
    EMBED_NAME=${embed_model:-}
    # Yes when the user picked the download from the model list.
    chosen_download=no
    # Other embedding models Ollama has, offered beside the recommended one.
    listed=$(printf '%s\n' "$models" | grep -vx "$EMBED_MODEL" || :)
    if [ -z "$EMBED_NAME" ] && [ "$1" = ask ] && [ -n "$listed" ]; then
        # The recommended model first, offered for download when missing.
        if printf '%s\n' "$models" | grep -qx "$EMBED_MODEL"; then
            first="$EMBED_MODEL (recommended)"
        else
            first="download $EMBED_MODEL ($EMBED_MODEL_SIZE, recommended)"
        fi
        count=1
        echo "Embedding models:" >/dev/tty
        printf '  1) %s\n' "$first" >/dev/tty
        if [ -n "$listed" ]; then
            printf '%s\n' "$listed" | while IFS= read -r name; do
                count=$((count + 1))
                printf '  %s) %s\n' "$count" "$name" >/dev/tty
            done
            count=$((1 + $(printf '%s\n' "$listed" | wc -l)))
        fi
        picked=$(choose "Model" "$count" 1)
        if [ "$picked" = 1 ]; then
            EMBED_NAME=$EMBED_MODEL
            chosen_download=yes
        else
            EMBED_NAME=$(printf '%s\n' "$listed" | sed -n "$((picked - 1))p")
        fi
    fi
    EMBED_NAME=${EMBED_NAME:-$EMBED_MODEL}
    if ! printf '%s\n' "$models" | grep -qx "$EMBED_NAME"; then
        if [ "$pull_model" = yes ] || [ "$chosen_download" = yes ]; then
            pull_model "$EMBED_NAME"
        elif [ "$1" = ask ]; then
            if [ "$EMBED_NAME" = "$EMBED_MODEL" ]; then
                question="Download $EMBED_NAME ($EMBED_MODEL_SIZE)?"
            else
                question="Download $EMBED_NAME?"
            fi
            ask "$question" y || return 1
            pull_model "$EMBED_NAME"
        else
            fail "Ollama has no model $EMBED_NAME; pass --pull-model to download it, or run: ollama pull $EMBED_NAME"
        fi
    fi
    EMBED_URL=$OLLAMA
}

# An embeddings address as given, checked to be HTTPS or plain http to this
# machine; `embedding set` adds the endpoint's path.
embed_address() {
    url=$(clean_url "$1")
    require_secure "$url" "the embeddings endpoint"
    printf '%s\n' "$url"
}

# Asks for another OpenAI-compatible endpoint: its address, the model and
# an API key, read hidden and saved to EMBED_KEY_PATH (mode 0600).
ask_endpoint() {
    printf 'Embeddings address (e.g. http://host:11434 or https://api.openai.com/v1): ' >/dev/tty
    IFS= read -r answer </dev/tty || answer=
    [ -n "$answer" ] || fail "no embeddings address given"
    EMBED_URL=$(embed_address "$answer")
    printf 'Model name: ' >/dev/tty
    IFS= read -r EMBED_NAME </dev/tty || EMBED_NAME=
    [ -n "$EMBED_NAME" ] || fail "no model name given"
    stty -echo </dev/tty
    trap 'stty echo </dev/tty; rm -rf "$tmp"; exit 1' INT TERM
    printf 'API key (input hidden; Enter for none): ' >/dev/tty
    IFS= read -r key </dev/tty || key=
    stty echo </dev/tty
    trap 'rm -rf "$tmp"; exit 1' INT TERM
    printf '\n' >/dev/tty
    if [ -n "$key" ]; then
        private_dir "$(dirname "$EMBED_KEY_PATH")"
        (
            umask 077
            printf '%s\n' "$key" >"$EMBED_KEY_PATH.$$"
            mv -f "$EMBED_KEY_PATH.$$" "$EMBED_KEY_PATH"
        ) || fail "cannot save the API key as $EMBED_KEY_PATH"
        EMBED_KEY=$EMBED_KEY_PATH
    fi
}

# Decides the embedding setting for a store on this machine. EMBED is keep
# (a rerun without embedding options), set, or off; set fills EMBED_URL,
# EMBED_NAME, EMBED_KEY and EMBED_DIMS. Nothing is installed or downloaded
# without the user's yes.
choose_embedding() {
    EMBED=keep EMBED_URL='' EMBED_NAME='' EMBED_KEY=$embed_key_file EMBED_DIMS=$embed_dimensions
    if [ "$no_embed" = yes ]; then
        EMBED=off
    elif [ -n "$embed_url" ]; then
        EMBED='set' EMBED_URL=$embed_url EMBED_NAME=$embed_model
    elif [ "$ollama_given" = yes ]; then
        use_ollama flags
        EMBED='set'
    elif [ "$UPGRADE" = yes ] || [ "$saved" = native ]; then
        return 0
    elif have_tty; then
        cat >/dev/tty <<'EOF'
Semantic search finds memories by meaning as well as by their words. It needs an embedding model:
  1) Ollama on this machine (recommended: private, no API key)
  2) Another OpenAI-compatible endpoint
  3) Off: keyword search only
EOF
        case $(choose "Semantic search" 3 1) in
            1) if use_ollama ask; then EMBED='set'; else EMBED=off; fi ;;
            2) ask_endpoint
                EMBED='set'
                ;;
            3) EMBED=off ;;
        esac
    elif ollama_running && ollama_embedding_models | grep -qx "$EMBED_MODEL"; then
        echo "Ollama has $EMBED_MODEL: using it for semantic search."
        EMBED='set' EMBED_URL=$OLLAMA EMBED_NAME=$EMBED_MODEL
    else
        EMBED=off
    fi
    if [ "$EMBED" = off ]; then
        echo "Semantic search is off (keyword search only). To turn it on later, run this installer again with --embed-model $EMBED_MODEL --pull-model, or with --embed-url URL --embed-model NAME."
    fi
}

# Sets up or upgrades the Docker server and points URL and the token at it.
# The installed binary writes the project, builds it, upgrades the store and
# copies the token; this script asks the questions and downloads the builds.
docker_server() {
    set -- --dir "$DOCKER_DIR" --name "$DOCKER_NAME" --port "$PORT"
    [ "$UPGRADE" = no ] || set -- "$@" --upgrade
    # Before any question: never take over another server's port or memories.
    "$tmp/user-memory" server check "$@" || exit 1
    choose_embedding
    server_target=$("$tmp/user-memory" server target) || exit 1
    printf 'Downloading the %s server build\n' "$server_target"
    download "$server_target" "$tmp/server"
    set -- "$@" --build "$tmp/server"
    # An upgrade keeps the shared-server choices it was not given again.
    if [ "$hosts_given" = yes ]; then
        while IFS= read -r host; do
            [ -z "$host" ] || set -- "$@" --allowed-host "$host"
        done <<EOF
$ALLOWED_HOSTS
EOF
    fi
    [ "$bind_given" = no ] || set -- "$@" --bind "$BIND"
    [ -z "$titles" ] || set -- "$@" --titles "$titles"
    URL=http://127.0.0.1:$PORT/mcp
    TOKEN_FILE=${XDG_CONFIG_HOME:-$HOME/.config}/user-memory/client-token
    if [ "$UPGRADE" = yes ] && [ -n "$SAVED_TOKEN" ] && [ -r "$SAVED_TOKEN" ]; then
        TOKEN_FILE=$SAVED_TOKEN
    else
        set -- "$@" --token-file "$TOKEN_FILE"
    fi
    "$tmp/user-memory" server install "$@" || exit 1
    # What the server now accepts, for the closing summary.
    ALLOWED_HOSTS=$(sed -n 's/^ *- --allowed-host=\(.*\)$/\1/p' "$DOCKER_DIR/compose.yaml")
    BIND=$(sed -n 's/^ *- "\([0-9.]*\):[0-9]*:8765"$/\1/p' "$DOCKER_DIR/compose.yaml" |
        grep -v '^127\.0\.0\.1$' | sed -n 1p)
    BIND=${BIND:-127.0.0.1}
}

# Applies the choice made in EMBED with the installed binary, which checks
# the endpoint, calibrates the model, saves the setting where the store
# lives and embeds the memories again (for the Docker server, inside its
# container). A store without a setting has none to turn off.
apply_embedding() {
    case $EMBED in
        set)
            set -- embedding set --url "$EMBED_URL" --model "$EMBED_NAME"
            [ -z "$EMBED_KEY" ] || set -- "$@" --key-file "$EMBED_KEY"
            [ -z "$EMBED_DIMS" ] || set -- "$@" --dimensions "$EMBED_DIMS"
            ;;
        off)
            [ "$UPGRADE" = yes ] || [ "$saved" = native ] || return 0
            set -- embedding off
            ;;
        *) return 0 ;;
    esac
    "$install_dir/user-memory" "$@" </dev/null ||
        fail "semantic search was not changed (above), though everything else is installed; $(bridge_hint)once that is fixed, run: '$install_dir/user-memory' $*"
}

# The likely cause when the Docker server cannot reach Ollama on this Linux
# machine, with its fix, ending in "; "; nothing otherwise. Docker Desktop
# (macOS, Windows) forwards host.docker.internal to this machine's loopback,
# where Ollama listens by default; native Docker on Linux points it at the
# bridge address instead. The installer never changes Ollama's setting, and
# never suggests every network (0.0.0.0) for it.
bridge_hint() {
    [ "$(uname -s)" = Linux ] && [ "$local_mode" = docker ] && [ "$EMBED_URL" = "$OLLAMA" ] || return 0
    authority=${OLLAMA#http://}
    authority=${authority%%/*}
    case $authority in
        *:*) port=${authority##*:} ;;
        *) port=80 ;;
    esac
    bridge=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null) || bridge=
    printf '%s\n' "$bridge" | grep -Eq '^[0-9]{1,3}(\.[0-9]{1,3}){3}$' || bridge=
    if [ -n "$bridge" ]; then
        printf 'on Linux the Docker server reaches Ollama at the Docker bridge address %s, where Ollama does not answer while it listens on 127.0.0.1, its default: set OLLAMA_HOST=%s:%s for the Ollama service (for example with sudo systemctl edit ollama), restart Ollama, and ' "$bridge" "$bridge" "$port"
    else
        printf 'on Linux the Docker server reaches Ollama at the Docker bridge address (docker network inspect bridge names it), where Ollama does not answer while it listens on 127.0.0.1, its default: set OLLAMA_HOST to that address and port %s for the Ollama service, restart Ollama, and ' "$port"
    fi
}

main() {
    install_dir=${HOME:?HOME is not set}/.local/bin
    url=
    local_mode=
    token_file=
    version=
    client=auto
    client_given=no
    server_only=no
    titles=
    PORT=8765
    port_given=no
    ALLOWED_HOSTS=
    hosts_given=no
    BIND=127.0.0.1
    bind_given=no
    embed_url=
    embed_model=
    embed_key_file=
    embed_dimensions=
    no_embed=no
    install_ollama=no
    pull_model=no
    while [ $# -gt 0 ]; do
        case $1 in
            --url | --local | --port | --token-file | --version | --install-dir | --client | --titles | --allowed-host | --bind | \
                --embed-url | --embed-model | --embed-key-file | --embed-dimensions)
                [ $# -ge 2 ] || fail "$1 needs a value"
                case $1 in
                    --url) url=$2 ;;
                    --local) local_mode=$2 ;;
                    --port) PORT=$2 port_given=yes ;;
                    --token-file) token_file=$(absolute "$2") ;;
                    --version) version=$2 ;;
                    --install-dir) install_dir=$(absolute "$2") ;;
                    --client) client=$2 client_given=yes ;;
                    --titles) titles=$(absolute "$2") ;;
                    --allowed-host)
                        printf '%s\n' "$2" | grep -Eq '^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$' ||
                            fail "--allowed-host takes a host name such as memory.example.com, without https:// or a path"
                        host=$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')
                        ALLOWED_HOSTS=$ALLOWED_HOSTS${ALLOWED_HOSTS:+
}$host
                        hosts_given=yes
                        ;;
                    --bind) BIND=$2 bind_given=yes ;;
                    --embed-url) embed_url=$2 ;;
                    --embed-model) embed_model=$2 ;;
                    --embed-key-file) embed_key_file=$(absolute "$2") ;;
                    --embed-dimensions) embed_dimensions=$2 ;;
                esac
                shift 2
                ;;
            --no-embed)
                no_embed=yes
                shift
                ;;
            --install-ollama)
                install_ollama=yes
                shift
                ;;
            --pull-model)
                pull_model=yes
                shift
                ;;
            --server-only)
                server_only=yes
                shift
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            *) fail "unknown option: $1 (see --help)" ;;
        esac
    done
    case $local_mode in
        '' | docker | native) ;;
        *) fail "--local takes docker or native" ;;
    esac
    [ -z "$titles" ] || [ -r "$titles" ] || fail "cannot read the titles map $titles"
    [ -z "$local_mode" ] || [ -z "$url" ] || fail "give either --url or --local, not both"
    if [ "$server_only" = yes ]; then
        [ -z "$url" ] ||
            fail "--server-only runs the memory server on this machine, while --url connects to one elsewhere; give one of them"
        [ "$local_mode" != native ] || fail "--server-only runs the server in Docker; leave out --local native"
        [ "$client_given" = no ] ||
            fail "--server-only connects no agent app; leave out --client, or use --local docker to connect this machine's agents to the server too"
    fi
    case $BIND in
        0.0.0.0 | :: | '[::]' | '*')
            fail "--bind $BIND would serve plain HTTP on every network; keep 127.0.0.1 for an HTTPS proxy on this machine, or bind a private or tunnel address that only the proxy reaches"
            ;;
    esac
    printf '%s\n' "$BIND" | grep -Eq '^[0-9]{1,3}(\.[0-9]{1,3}){3}$' ||
        fail "--bind takes an IPv4 address such as 127.0.0.1 or 10.0.0.5"
    case $PORT in
        '' | *[!0-9]*) fail "--port takes a port number" ;;
    esac
    if ! { [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ]; }; then
        fail "--port takes a port number"
    fi
    if [ -n "$version" ]; then
        printf '%s\n' "$version" | grep -Eq '^v[0-9][A-Za-z0-9._-]*$' ||
            fail "--version takes a release tag such as v0.3.0"
    fi
    embed_given=no
    ollama_given=no
    if [ -n "$embed_url$embed_model$embed_key_file$embed_dimensions" ] ||
        [ "$no_embed$install_ollama$pull_model" != nonono ]; then
        embed_given=yes
    fi
    if [ "$no_embed" = yes ]; then
        [ -z "$embed_url$embed_model$embed_key_file$embed_dimensions" ] &&
            [ "$install_ollama$pull_model" = nono ] ||
            fail "--no-embed goes without the other semantic search options"
    elif [ -n "$embed_url" ]; then
        [ -n "$embed_model" ] || fail "--embed-url needs --embed-model"
        [ "$install_ollama$pull_model" = nono ] ||
            fail "--install-ollama and --pull-model are for Ollama on this machine, not --embed-url"
        embed_url=$(embed_address "$embed_url")
    elif [ -n "$embed_key_file" ]; then
        fail "--embed-key-file goes with --embed-url"
    elif [ "$embed_given" = yes ]; then
        ollama_given=yes
    fi
    if [ -n "$embed_dimensions" ]; then
        case $embed_dimensions in
            '' | *[!0-9]* | 0*) fail "--embed-dimensions takes a positive whole number" ;;
        esac
    fi
    [ -z "$embed_key_file" ] || [ -r "$embed_key_file" ] || fail "cannot read the API key file $embed_key_file"
    releases=$(clean_url "${USER_MEMORY_RELEASES:-https://github.com/l7aromeo/user-memory/releases}")
    require_secure "$releases" "the releases address"
    OLLAMA=$(clean_url "${USER_MEMORY_OLLAMA_URL:-http://127.0.0.1:11434}")
    loopback_http "$OLLAMA" || fail "USER_MEMORY_OLLAMA_URL must be plain http on this machine"
    # Where an API key typed for another embeddings endpoint is saved.
    EMBED_KEY_PATH=${XDG_CONFIG_HOME:-$HOME/.config}/user-memory/embedding-key
    OLLAMA_INSTALL_SCRIPT=$(clean_url "${USER_MEMORY_OLLAMA_INSTALL_SCRIPT:-$OLLAMA_INSTALL_SCRIPT}")
    require_secure "$OLLAMA_INSTALL_SCRIPT" "Ollama's install script"
    DOCKER_NAME=${USER_MEMORY_DOCKER_NAME:-user-memory}
    printf '%s\n' "$DOCKER_NAME" | grep -Eq '^[a-z0-9][a-z0-9_-]*$' ||
        fail "USER_MEMORY_DOCKER_NAME must be a lowercase Docker project name"
    DOCKER_DIR=${XDG_DATA_HOME:-$HOME/.local/share}/user-memory/docker
    [ "$DOCKER_NAME" = user-memory ] || DOCKER_DIR=$DOCKER_DIR-$DOCKER_NAME

    # What an earlier install set up: a shared server (possibly the Docker
    # one this installer runs), or a local store. The installed binary reads
    # its own setup; the environment must not override it.
    saved_url=
    SAVED_TOKEN=
    saved=none
    if [ -x "$install_dir/user-memory" ]; then
        if found=$(env -u USER_MEMORY_URL -u USER_MEMORY_TOKEN_FILE -u USER_MEMORY_DB \
            "$install_dir/user-memory" endpoint 2>/dev/null); then
            saved_url=$(printf '%s\n' "$found" | sed -n 1p)
            SAVED_TOKEN=$(printf '%s\n' "$found" | sed -n 2p)
            saved=remote
            if [ -f "$DOCKER_DIR/compose.yaml" ]; then
                docker_port=$(sed -n 's/^ *- "127\.0\.0\.1:\([0-9]*\):8765"$/\1/p' "$DOCKER_DIR/compose.yaml")
                [ "$saved_url" != "http://127.0.0.1:$docker_port/mcp" ] || saved=docker
            fi
        elif [ -f "$install_dir/.user-memory.json" ]; then
            saved=native
        fi
    fi
    # A server-only host stays one until --client, --url or --local native
    # makes it a client install.
    if [ "$saved" = docker ] && [ -z "$url" ] && [ "$local_mode" != native ] && [ "$client_given" = no ] &&
        grep -q '"server_only": true' "$install_dir/.user-memory.json" 2>/dev/null; then
        server_only=yes
    fi
    [ "$server_only" = no ] || local_mode=${local_mode:-docker}

    # Where the memory lives: flag, environment, earlier install, then ask.
    [ -n "$url" ] || [ -n "$local_mode" ] || url=${USER_MEMORY_URL:-}
    UPGRADE=no
    if [ -z "$url" ] && [ -z "$local_mode" ]; then
        case $saved in
            remote)
                url=$saved_url
                printf 'Memory server: %s (from the earlier install)\n' "$url"
                ;;
            docker)
                local_mode=docker
                printf 'Upgrading the Docker memory server in %s\n' "$DOCKER_DIR"
                ;;
            native)
                local_mode=native
                echo "Upgrading the local memory store install"
                ;;
        esac
    fi
    if [ -z "$url" ] && [ -z "$local_mode" ]; then
        have_tty || fail "no server address: pass --url, or --local docker|native"
        if ask "Do you already have a user-memory server?" n; then
            printf 'Memory server address (e.g. https://memory.example.com or http://127.0.0.1:8765): ' >/dev/tty
            IFS= read -r url </dev/tty || url=
            [ -n "$url" ] || fail "no server address given"
        elif ! ask "Set up a memory server on this machine?" y; then
            echo "Nothing was installed. Run this installer again with --url URL once you have a server, or with --local docker|native to set one up here."
            exit 0
        elif docker_ready; then
            if ask "Run it in Docker?" y; then
                local_mode=docker
            else
                local_mode=native
            fi
        else
            echo "Docker is not available here (no docker command, or docker info failed)."
            if ask "Keep the memories in a local store on this machine instead?" y; then
                local_mode=native
            else
                echo "Nothing was installed. Start Docker and run this installer again, or pass --local native."
                exit 0
            fi
        fi
    fi
    if [ -z "$local_mode" ]; then
        URL=$(endpoint_of "$url")
        require_secure "$URL" "the memory server"
        [ "$embed_given" = no ] ||
            fail "semantic search is set where the memory server runs; pass these options to the installer on that machine"
    fi
    if [ "$local_mode" = docker ] && [ "$saved" = docker ]; then
        # The Docker server this installer set up earlier: upgrade it in place.
        UPGRADE=yes
        [ "$port_given" = yes ] || PORT=$docker_port
    fi
    if [ "$local_mode" != docker ] && { [ "$hosts_given" = yes ] || [ "$bind_given" = yes ]; }; then
        fail "--allowed-host and --bind set up a shared Docker server; use them with --local docker"
    fi
    command -v curl >/dev/null 2>&1 || fail "curl is required"

    if [ -n "$version" ]; then
        ASSETS=$releases/download/$version
    else
        ASSETS=$releases/latest/download
    fi
    target=$(detect_target)
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/user-memory-install.XXXXXX")
    # macOS /bin/sh (bash 3.2) exits 0 after an unset variable when an EXIT
    # trap is set, so success is only what reaches the end of main.
    finished=no
    trap 'rm -rf "$tmp"; [ "$finished" = yes ] || exit 1' EXIT
    trap 'rm -rf "$tmp"; exit 1' INT TERM

    printf 'Downloading user-memory %s for %s\n' "${version:-(latest)}" "$target"
    download "$target" "$tmp/user-memory"
    echo "Checksum: OK"

    # Setup installs the verified client into install_dir in one rename,
    # connects the clients (none on a server-only host) and checks the
    # server; doctor confirms the result.
    # Neither reads standard input, which may still be the piped script.
    case $local_mode in
        docker)
            docker_server
            if [ "$server_only" = yes ]; then
                "$tmp/user-memory" setup --url "$URL" --token-file "$TOKEN_FILE" \
                    --install-dir "$install_dir" --server-only </dev/null
            else
                "$tmp/user-memory" setup --url "$URL" --token-file "$TOKEN_FILE" \
                    --install-dir "$install_dir" --client "$client" </dev/null
            fi
            apply_embedding
            ;;
        native)
            choose_embedding
            "$tmp/user-memory" setup --install-dir "$install_dir" --client "$client" \
                ${titles:+--titles} ${titles:+"$titles"} </dev/null
            apply_embedding
            ;;
        *)
            [ -z "$titles" ] ||
                fail "--titles upgrades a store on this machine; a remote server's store is upgraded on the server"
            if [ -n "$token_file" ]; then
                [ -r "$token_file" ] || fail "cannot read the token file $token_file"
                TOKEN_FILE=$token_file
            else
                TOKEN_FILE=${XDG_CONFIG_HOME:-$HOME/.config}/user-memory/client-token
                if [ -n "${USER_MEMORY_TOKEN:-}" ]; then
                    save_token "$USER_MEMORY_TOKEN"
                elif [ "$URL" = "$saved_url" ] && [ -n "$SAVED_TOKEN" ] && [ -r "$SAVED_TOKEN" ]; then
                    # Re-running for the same server keeps the token it already uses.
                    TOKEN_FILE=$SAVED_TOKEN
                else
                    prompt_token
                fi
            fi
            "$tmp/user-memory" setup --url "$URL" --token-file "$TOKEN_FILE" \
                --install-dir "$install_dir" --client "$client" </dev/null
            ;;
    esac
    # Everything is installed whatever doctor finds, so how to use it is
    # said either way. Doctor's status 3 means nothing is broken but the
    # user has a step left, which the run repeats last and does not fail
    # on; any other failure fails the run after the summary.
    doctor_status=0
    # The release just installed is the newest one there is to check for.
    USER_MEMORY_NO_UPDATE_CHECK=1 "$install_dir/user-memory" doctor </dev/null >"$tmp/doctor" || doctor_status=$?
    cat "$tmp/doctor"

    if [ "$local_mode" = docker ]; then
        cat <<EOF

The memory server runs in Docker on 127.0.0.1:$PORT, from $DOCKER_DIR.
  Stop or start it:  docker compose -f $DOCKER_DIR/compose.yaml stop   (or start)
  Logs:              docker compose -f $DOCKER_DIR/compose.yaml logs memory
  Upgrade:           run this installer again; it rebuilds the server from the
                     newer release and upgrades the store, backing it up first.
Its memories live in the Docker volume $DOCKER_NAME-data.
EOF
        if [ -n "$ALLOWED_HOSTS" ]; then
            first=$(printf '%s\n' "$ALLOWED_HOSTS" | sed -n 1p)
            printf '\nOther machines reach it through your HTTPS proxy, which forwards to http://%s:%s, at:\n' "$BIND" "$PORT"
            printf '%s\n' "$ALLOWED_HOSTS" | sed 's|^|  https://|'
            cat <<EOF
On each of them, save this machine's access token (from $TOKEN_FILE) in a
file with mode 0600, then run:
  curl -fsSL $releases/latest/download/install.sh | sh -s -- --url https://$first --token-file /path/to/token
EOF
        fi
        if [ "$server_only" = yes ]; then
            printf '
This machine runs only the memory server; no agent app here uses it. Its access
token is in %s; every machine that uses the server needs a copy.
' "$TOKEN_FILE"
            [ -n "$ALLOWED_HOSTS" ] ||
                printf 'Other machines reach it through an HTTPS proxy: put one in front of it, then run
this installer again with --allowed-host HOST (and --bind ADDR for a proxy on
another machine) to print their install command.
'
        fi
    fi
    case :$PATH: in
        *:"$install_dir":*) ;;
        *) printf '\nAdd %s to PATH to run user-memory from a shell.\n' "$install_dir" ;;
    esac
    if grep -q '^Action needed: ' "$tmp/doctor"; then
        printf '\nStill to do before memory works everywhere:\n'
        sed -n 's/^Action needed: /  /p' "$tmp/doctor"
    fi
    if [ "$doctor_status" -ne 0 ] && [ "$doctor_status" -ne 3 ]; then
        printf "\nThe install finished, but doctor found a problem (above); fix it, then run '%s' doctor again.\n" "$install_dir/user-memory" >&2
        exit 1
    fi
    finished=yes
}

main "$@"
