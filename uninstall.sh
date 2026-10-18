#!/bin/sh
# Removes user-memory from this machine: what setup installed for the
# clients (through the installed binary's `uninstall`), the binary, the
# Docker server the installer set up, and anything left. Memories stay
# unless the user confirms deleting them. A shared server elsewhere is never
# touched. Runs from the release as
#   curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.sh | sh

set -eu

fail() {
    printf 'user-memory uninstall: %s\n' "$*" >&2
    exit 2
}

usage() {
    cat <<'EOF'
Remove user-memory from this machine.

Usage: curl -fsSL https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.sh | sh -s -- [options]

  --dry-run             list what would be removed or kept; change nothing
  --purge               delete the memories too (the local store and its
                        backups, or the Docker server's volume); on a
                        terminal it asks anyway
  --yes                 with --purge: delete without asking
  --no-export           with --purge: skip the export taken first
  --client NAME         auto (default), codex, claude or both
  --install-dir DIR     where the binary is (default ~/.local/bin)

Memories are kept unless you confirm: on a terminal you are asked (the
default keeps them, and deleting offers an export first and asks you to
type delete); without one, --purge --yes deletes them.
EOF
}

absolute() {
    case $1 in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$PWD" "$1" ;;
    esac
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

# Prints a removed item, or the item a dry run would remove.
removed() {
    changed=$((changed + 1))
    if [ "$dry_run" = yes ]; then
        printf '  Would remove: %s\n' "$1"
    else
        printf '  Removed: %s\n' "$1"
    fi
}

# Reports an item of ours that could not be removed, with what to do; the
# run then exits 1 after everything else.
left() {
    printf '  Left: %s\n' "$1"
    status=1
}

# Whether PATH is a link; nothing is written through one.
is_link() {
    [ -L "$1" ]
}

# Decides about memories at WHERE (described for the user): sets
# delete_memories and export_memories. Never deletes without --purge --yes
# or a typed confirmation on the terminal.
decide_memories() {
    delete_memories=no
    export_memories=no
    if [ "$dry_run" = yes ]; then
        printf '  Would keep: your memories (%s), unless you confirm deleting them\n' "$1"
        return
    fi
    if [ "$purge" = yes ] && [ "$yes" = yes ]; then
        delete_memories=yes
        [ "$no_export" = yes ] || export_memories=yes
        return
    fi
    have_tty || return 0
    if ask "Keep your memories ($1)?" y; then
        return
    fi
    if [ "$no_export" = no ] && ask "Export them to a file in this folder first?" y; then
        export_memories=yes
    fi
    printf 'Type delete to delete them for good: ' >/dev/tty
    IFS= read -r typed </dev/tty || typed=
    if [ "$typed" = delete ]; then
        delete_memories=yes
    else
        export_memories=no
    fi
}

# Deletes the automatic backups of the store STORE from the backups folder
# beside it, and only those: STEM-YYYY-MM-DD.EXT, the failure note and
# copies cut short. Then the folder, once nothing else is in it.
remove_backups() {
    backups_dir=$(dirname "$1")/backups
    [ -d "$backups_dir" ] || return 0
    file=${1##*/}
    case $file in
        ?*.*) stem=${file%.*} ext=.${file##*.} ;;
        *) stem=$file ext= ;;
    esac
    for backup in "$backups_dir"/* "$backups_dir"/.[!.]*; do
        case ${backup##*/} in
            "$stem"-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]"$ext" | last-failure | \
                ."$stem"-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]"$ext".partial-*)
                [ -f "$backup" ] && ! is_link "$backup" && rm -f "$backup" ;;
        esac
    done
    rmdir "$backups_dir" 2>/dev/null || true
}

# The client side without a working binary: removes the files user-memory
# owns outright, and names the edits inside the user's own files that only
# the binary makes safely.
fallback() {
    changed_before=$changed
    left_before=$status
    claude_dir=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
    codex_dir=${CODEX_HOME:-$HOME/.codex}
    if [ "$client" != codex ] && command -v claude >/dev/null 2>&1 &&
        claude mcp get user-memory 2>/dev/null | grep -q 'Scope: User config'; then
        if [ "$dry_run" = yes ] || claude mcp remove --scope user user-memory >/dev/null 2>&1; then
            removed "Claude Code registration"
        else
            left "Claude Code registration; run: claude mcp remove --scope user user-memory"
        fi
    fi
    if [ "$client" != claude ] && command -v codex >/dev/null 2>&1 &&
        codex mcp get user-memory --json >/dev/null 2>&1; then
        if [ "$dry_run" = yes ] || codex mcp remove user-memory >/dev/null 2>&1; then
            removed "Codex registration"
        else
            left "Codex registration; run: codex mcp remove user-memory"
        fi
    fi
    for file in "$claude_dir/settings.json" "$codex_dir/hooks.json"; do
        if [ -f "$file" ] && grep -q 'user-memory.*session-start\|user-memory.*prompt-submit' "$file"; then
            left "the user-memory session hooks in $file; install user-memory again and run this uninstall, or remove those two hook entries by hand"
        fi
    done
    for file in "$codex_dir/AGENTS.md" "$codex_dir/AGENTS.override.md"; do
        if [ -f "$file" ] && grep -q '<!-- user-memory:start -->' "$file"; then
            left "the user-memory rules in $file; remove the block from <!-- user-memory:start --> to <!-- user-memory:end -->"
        fi
    done
    rules=$claude_dir/rules/user-memory.md
    if [ -f "$rules" ] && head -n 1 "$rules" | grep -q '^<!-- Generated by user-memory'; then
        if is_link "$rules"; then
            left "$rules is a symbolic link user-memory does not write through; remove the generated file it points to yourself"
        else
            [ "$dry_run" = yes ] || rm -f "$rules"
            removed "the user-memory rules in $rules"
        fi
    fi
    skill=$HOME/.agents/skills/user-memory
    if [ -f "$skill/.user-memory-managed" ] && ! is_link "$skill"; then
        link=$claude_dir/skills/user-memory
        if is_link "$link"; then
            [ "$dry_run" = yes ] || rm -f "$link"
        fi
        [ "$dry_run" = yes ] || rm -rf "$skill"
        removed "skill ($skill)"
    fi
    ignore=$(git config --global --type=path --get core.excludesFile 2>/dev/null) ||
        ignore=${XDG_CONFIG_HOME:-$HOME/.config}/git/ignore
    if [ -n "$ignore" ] && [ -f "$ignore" ] && grep -qx '# user-memory' "$ignore"; then
        if is_link "$ignore"; then
            left "the user-memory lines in $ignore, a symbolic link; remove # user-memory, .user-memory and .user-memory-key there yourself"
        else
            if [ "$dry_run" = no ]; then
                kept=$(grep -vx -e '# user-memory' -e '.user-memory' -e '.user-memory-key' "$ignore" || :)
                if [ -z "$(printf '%s' "$kept" | tr -d '[:space:]')" ]; then
                    rm -f "$ignore"
                else
                    printf '%s\n' "$kept" >"$ignore"
                fi
            fi
            removed "the global git ignore lines ($ignore)"
        fi
    fi
    state=${XDG_STATE_HOME:-$HOME/.local/state}/user-memory
    if [ -d "$state" ] && ! is_link "$state"; then
        [ "$dry_run" = yes ] || rm -rf "$state"
        removed "session snapshots ($state)"
    fi
    token=${XDG_CONFIG_HOME:-$HOME/.config}/user-memory/client-token
    if [ -f "$token" ] && ! is_link "$token"; then
        if [ "$dry_run" != yes ] && rm -f "$token"; then
            rmdir "$(dirname "$token")" 2>/dev/null || :
        fi
        removed "the access token $token"
    fi
    receipt=$install_dir/.user-memory.json
    store=
    if [ -f "$receipt" ]; then
        store=$(sed -n 's/^ *"db": *"\(.*\)",*$/\1/p' "$receipt")
        [ "$dry_run" = yes ] || rm -f "$receipt"
        removed "saved setup $receipt"
    fi
    # Without a saved setup, the store the binary would use: USER_MEMORY_DB,
    # else its default, which XDG_DATA_HOME does not move.
    [ -n "$store" ] || store=${USER_MEMORY_DB:-$HOME/.local/share/user-memory/memories.sqlite3}
    if [ -f "$store" ]; then
        decide_memories "$store"
        if [ "$delete_memories" = yes ] && [ "$export_memories" = yes ]; then
            left "your memories at $store were kept: exporting them needs a working user-memory; delete them anyway with --purge --yes --no-export"
        elif [ "$delete_memories" = yes ]; then
            rm -f "$store" "$store-wal" "$store-shm" "$store.lock" "$store".*.bak
            remove_backups "$store"
            rmdir "$(dirname "$store")" 2>/dev/null || :
            removed "your memories ($store)"
        elif [ "$dry_run" = no ]; then
            kept_store=$store
        fi
    fi
    if [ "$changed" -eq "$changed_before" ] && [ "$status" -eq "$left_before" ]; then
        echo "  Nothing to remove."
    fi
    if [ -n "${kept_store:-}" ]; then
        printf '\nYour memories were kept: %s. To delete them later: rm %s*\n' "$kept_store" "$kept_store"
    fi
}

# Stops and removes the Docker server the installer set up, and its volume
# only when the user confirmed deleting the memories in it: through the
# installed binary's `server remove` when it runs, else directly.
docker_server() {
    [ -f "$DOCKER_DIR/compose.yaml" ] || return 0
    if [ "$binary_runs" = yes ]; then
        set -- --dir "$DOCKER_DIR" --name "$DOCKER_NAME"
        [ "$dry_run" = no ] || set -- "$@" --dry-run
        [ "$purge" = no ] || set -- "$@" --purge
        [ "$yes" = no ] || set -- "$@" --yes
        [ "$no_export" = no ] || set -- "$@" --no-export
        remove_status=0
        "$binary" server remove "$@" || remove_status=$?
        # 1 means items were left and named; anything else, the binary
        # failed, and the server is removed directly.
        case $remove_status in
            0) return ;;
            1)
                status=1
                return
                ;;
            *) echo "  The installed user-memory failed to remove the Docker server; removing it directly." ;;
        esac
    fi
    volume=$DOCKER_NAME-data
    if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
        left "the Docker memory server in $DOCKER_DIR (Docker is not running); start Docker and run this again"
        return
    fi
    decide_memories "the Docker volume $volume"
    if [ "$dry_run" = yes ]; then
        removed "the Docker memory server (container, image and network) and $DOCKER_DIR"
        return
    fi
    if [ "$export_memories" = yes ]; then
        file=$PWD/user-memory-export-$(date -u +%Y%m%dT%H%M%SZ).json
        if (umask 077 && docker compose -f "$DOCKER_DIR/compose.yaml" run --rm -T memory export - >"$file"); then
            printf '  Exported your memories to %s (mode 0600)\n' "$file"
        else
            rm -f "$file"
            left "the Docker memory server and its memories: the export failed, so nothing was deleted; fix Docker and run this again, or pass --no-export"
            return
        fi
    fi
    if [ "$delete_memories" = yes ]; then
        down_volumes=-v
    else
        down_volumes=
    fi
    # shellcheck disable=SC2086
    if docker compose -f "$DOCKER_DIR/compose.yaml" down --rmi local $down_volumes >/dev/null 2>&1; then
        removed "the Docker memory server (container, image and network)"
        [ "$delete_memories" = no ] || removed "your memories (the Docker volume $volume)"
        rm -rf "$DOCKER_DIR"
        removed "$DOCKER_DIR"
        [ "$delete_memories" = yes ] ||
            printf '\nYour memories were kept in the Docker volume %s. To delete them later: docker volume rm %s\n' "$volume" "$volume"
    else
        left "the Docker memory server; run: docker compose -f $DOCKER_DIR/compose.yaml down --rmi local"
    fi
}

main() {
    install_dir=${HOME:?HOME is not set}/.local/bin
    dry_run=no
    purge=no
    yes=no
    no_export=no
    client=auto
    while [ $# -gt 0 ]; do
        case $1 in
            --dry-run) dry_run=yes ;;
            --purge) purge=yes ;;
            --yes) yes=yes ;;
            --no-export) no_export=yes ;;
            --client | --install-dir)
                [ $# -ge 2 ] || fail "$1 needs a value"
                case $1 in
                    --client) client=$2 ;;
                    --install-dir) install_dir=$(absolute "$2") ;;
                esac
                shift
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            *) fail "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
    case $client in
        auto | codex | claude | both) ;;
        *) fail "--client takes auto, codex, claude or both" ;;
    esac
    [ "$purge" = yes ] || [ "$yes" = no ] || fail "--yes goes with --purge"
    [ "$purge" = yes ] || [ "$no_export" = no ] || fail "--no-export goes with --purge"
    if [ "$purge" = yes ] && [ "$yes" = no ] && [ "$dry_run" = no ] && ! have_tty; then
        fail "--purge deletes your memories; without a terminal to confirm, it also needs --yes. Nothing was changed."
    fi
    DOCKER_NAME=${USER_MEMORY_DOCKER_NAME:-user-memory}
    DOCKER_DIR=${XDG_DATA_HOME:-$HOME/.local/share}/user-memory/docker
    [ "$DOCKER_NAME" = user-memory ] || DOCKER_DIR=$DOCKER_DIR-$DOCKER_NAME
    status=0
    changed=0
    binary=$install_dir/user-memory

    # The client side first, so no agent starts a session against a
    # half-removed install: the installed binary knows every path and format.
    # This script removes the binary afterwards, so it need not be named.
    set -- --client "$client" --install-dir "$install_dir" --binary-removed-after
    [ "$dry_run" = no ] || set -- "$@" --dry-run
    [ "$purge" = no ] || set -- "$@" --purge
    [ "$yes" = no ] || set -- "$@" --yes
    [ "$no_export" = no ] || set -- "$@" --no-export
    binary_runs=no
    if [ -x "$binary" ] && "$binary" --version >/dev/null 2>&1; then
        binary_runs=yes
        binary_status=0
        "$binary" uninstall "$@" || binary_status=$?
        # 1 means items were left and named; anything else, the binary
        # failed, and what is left is removed directly.
        case $binary_status in
            0) ;;
            1) status=1 ;;
            *)
                echo "  The installed user-memory failed; removing what is left directly."
                binary_runs=no
                fallback
                ;;
        esac
    else
        echo "Uninstalling user memory…"
        [ ! -e "$binary" ] || echo "  The installed user-memory does not run; removing what is left directly."
        fallback
    fi

    # Then the server this installer set up, then the binary.
    docker_server
    if [ -e "$binary" ] || is_link "$binary"; then
        [ "$dry_run" = yes ] || rm -f "$binary"
        removed "the user-memory binary $binary"
    fi
    if [ "$dry_run" = no ]; then
        rmdir "${XDG_DATA_HOME:-$HOME/.local/share}/user-memory" 2>/dev/null || :
    fi
    if [ "$status" -ne 0 ]; then
        printf '\nUninstall finished, but some items were left (above, each with what to do).\n' >&2
    fi
    exit "$status"
}

main "$@"
