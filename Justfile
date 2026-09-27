# Rebuild and restart the TUI when active Rust sources change.
dev:
    watchexec --restart --stop-signal SIGTERM --stop-timeout 2s --wrap-process=none --watch src --watch migrations --watch Cargo.toml --watch Cargo.lock --shell=none -- sh -c 'cargo build && exec ./target/debug/altitude'

# Create an immutable, forward-only SQL migration.
[positional-arguments]
migration name:
    #!/usr/bin/env bash
    set -euo pipefail

    name="{{name}}"
    if [[ ! "$name" =~ ^[a-z][a-z0-9_]*$ ]]; then
        echo "migration names must be lower_snake_case" >&2
        exit 1
    fi

    file="migrations/$(date -u +%Y%m%d%H%M%S)_${name}.sql"
    touch "$file"
    echo "created $file"

# Open altitude's local database with sqlite3.
db:
    #!/usr/bin/env bash
    set -euo pipefail

    case "$(uname -s)" in
        Darwin) database="$HOME/Library/Application Support/altitude/altitude.db" ;;
        Linux) database="${XDG_DATA_HOME:-$HOME/.local/share}/altitude/altitude.db" ;;
        *)
            echo "just db does not support $(uname -s) yet" >&2
            exit 1
            ;;
    esac

    if [[ ! -f "$database" ]]; then
        echo "altitude has not created its database yet; run altitude once first" >&2
        exit 1
    fi

    exec sqlite3 "$database"

# Tail altitude's application log.
logs:
    #!/usr/bin/env bash
    set -euo pipefail

    case "$(uname -s)" in
        Darwin) logs_dir="$HOME/Library/Caches/altitude/logs" ;;
        Linux) logs_dir="${XDG_CACHE_HOME:-$HOME/.cache}/altitude/logs" ;;
        *)
            echo "just logs does not support $(uname -s) yet" >&2
            exit 1
            ;;
    esac

    exec tail -n 100 -F "$logs_dir/altitude.log"

# Remove only Altitude's task database for a fresh local task history.
reset:
    #!/usr/bin/env bash
    set -euo pipefail

    case "$(uname -s)" in
        Darwin) database="$HOME/Library/Application Support/altitude/altitude.db" ;;
        Linux) database="${XDG_DATA_HOME:-$HOME/.local/share}/altitude/altitude.db" ;;
        *)
            echo "just reset does not support $(uname -s) yet" >&2
            exit 1
            ;;
    esac

    echo "This removes only Altitude's task database:"
    echo "  $database"
    echo "Snapshots, persistent profiles, isolation state, and logs are kept."
    read -r -p "Make sure altitude is not running. Continue? [y/N] " answer
    if [[ "$answer" != "y" && "$answer" != "Y" ]]; then
        echo "cancelled"
        exit 0
    fi

    rm -f -- "$database" "${database}-wal" "${database}-shm"
    echo "altitude task database removed"

# Remove all local altitude state for a fresh development start.
nuke:
    #!/usr/bin/env bash
    set -euo pipefail

    case "$(uname -s)" in
        Darwin)
            data_dir="$HOME/Library/Application Support/altitude"
            cache_dir="$HOME/Library/Caches/altitude"
            ;;
        Linux)
            data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/altitude"
            cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/altitude"
            ;;
        *)
            echo "just nuke does not support $(uname -s) yet" >&2
            exit 1
            ;;
    esac

    echo "This removes altitude's local data and cache:"
    echo "  $data_dir"
    echo "  $cache_dir"
    read -r -p "Make sure altitude is not running. Continue? [y/N] " answer
    if [[ "$answer" != "y" && "$answer" != "Y" ]]; then
        echo "cancelled"
        exit 0
    fi

    rm -rf -- "$data_dir" "$cache_dir"
    echo "altitude local state removed"

# Run the msb CLI against altitude's isolation state.
[positional-arguments]
msb *args:
    #!/usr/bin/env bash
    set -euo pipefail

    case "$(uname -s)" in
        Darwin)
            data_home="$HOME/Library/Application Support/altitude/isolation"
            socket_safe_home="$HOME/Library/Caches/altitude/msb"
            ;;
        Linux)
            data_home="${XDG_DATA_HOME:-$HOME/.local/share}/altitude/isolation"
            socket_safe_home="${XDG_CACHE_HOME:-$HOME/.cache}/altitude/msb"
            ;;
        *)
            echo "just msb does not support $(uname -s) yet" >&2
            exit 1
            ;;
    esac

    binary="$data_home/bin/msb"
    if [[ ! -x "$binary" ]]; then
        echo "altitude has not installed the microsandbox runtime yet; run altitude once first" >&2
        exit 1
    fi

    MSB_HOME="$socket_safe_home" exec "$binary" "$@"
