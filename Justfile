# Run the module tests. The ones under `task::run::tests` drive a real sandbox, so
# they need a Linux host with mount namespaces (the testbed isolation); everything
# else runs anywhere.
test:
    cargo test --locked --bin altitude

# Drive the real TUI from a journey file, for looking at or exercising the
# interface without a terminal of your own.
[positional-arguments]
tui script:
    python3 scripts/tui.py --script {{script}}

# Rebuild and restart the TUI when active Rust sources change.
dev:
    watchexec --restart --stop-signal SIGTERM --stop-timeout 2s --wrap-process=none --watch src --watch migrations --watch Cargo.toml --watch Cargo.lock --shell=none -- sh -c 'cargo build && exec ./target/debug/altitude'

# Run a debug build against an empty state under /tmp, so a demo never reads the
# real installation. `just demo clean` throws that state away first.
[positional-arguments]
demo action='':
    #!/usr/bin/env bash
    set -euo pipefail

    root="${ALTITUDE_DEMO_ROOT:-/tmp/altitude-demo}"
    case "{{action}}" in
        '') ;;
        clean) rm -rf -- "$root" ;;
        *)
            echo "usage: just demo [clean]" >&2
            exit 1
            ;;
    esac
    mkdir -p "$root"/{home,config,data,cache}

    cargo build

    # env -i drops the real installation's variables: the three directories below,
    # and the tool logins HOME and the settings pages reach for.
    environment=(
        HOME="$root/home"
        PATH="$PATH"
        TERM="${TERM:-xterm-256color}"
        ALTITUDE_CONFIG_DIR="$root/config"
        ALTITUDE_DATA_DIR="$root/data"
        ALTITUDE_CACHE_DIR="$root/cache"
    )
    if [[ -n "${LOG_LEVEL:-}" ]]; then
        environment+=(LOG_LEVEL="$LOG_LEVEL")
    fi
    # Trust belongs to the machine, not to this installation: a network that inspects
    # TLS needs the same roots inside the demo sandbox as a real run would carry, and
    # dropping them here is what makes npm fail to verify a registry it can reach.
    for name in NODE_EXTRA_CA_CERTS SSL_CERT_FILE CURL_CA_BUNDLE; do
        if [[ -n "${!name:-}" ]]; then
            environment+=("$name=${!name}")
        fi
    done

    echo "altitude is running against $root"
    exec env -i "${environment[@]}" ./target/debug/altitude

# Build a release binary using the locked dependency versions.
build:
    cargo build --release --locked

# Render the app icon from its source, for `brew install librsvg`.
icon:
    rsvg-convert -w 512 -h 512 assets/icon.svg -o assets/icon.png

# Run a built release binary
run:
    ./target/release/altitude

# Regenerate Formula/altitude.rb from a released tag's published checksums.
bump-formula tag:
    #!/usr/bin/env bash
    set -euo pipefail

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    gh release download "{{tag}}" --pattern SHA256SUMS --dir "$tmp"
    ./scripts/homebrew-formula "{{tag}}" "$tmp/SHA256SUMS"

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

    database="${ALTITUDE_DATA_DIR:-$(dirname "$database")}/altitude.db"

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

    logs_dir="${ALTITUDE_CACHE_DIR:-$(dirname "$logs_dir")}/logs"

    exec tail -n 100 -F "$logs_dir/altitude.log"

# Remove only altitude's task database for a fresh local task history.
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

    database="${ALTITUDE_DATA_DIR:-$(dirname "$database")}/altitude.db"

    echo "This removes only altitude's task database:"
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

    data_dir="${ALTITUDE_DATA_DIR:-$data_dir}"
    cache_dir="${ALTITUDE_CACHE_DIR:-$cache_dir}"

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

    data_home="${ALTITUDE_DATA_DIR:-$(dirname "$data_home")}/isolation"
    socket_safe_home="${ALTITUDE_CACHE_DIR:-$(dirname "$socket_safe_home")}/msb"

    binary="$data_home/bin/msb"
    if [[ ! -x "$binary" ]]; then
        echo "altitude has not installed the microsandbox runtime yet; run altitude once first" >&2
        exit 1
    fi

    MSB_HOME="$socket_safe_home" exec "$binary" "$@"
