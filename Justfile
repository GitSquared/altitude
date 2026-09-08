# Rebuild and restart the TUI when active Rust sources change.
dev:
    watchexec --restart --stop-signal SIGTERM --stop-timeout 2s --wrap-process=none --watch src --watch Cargo.toml --watch Cargo.lock --shell=none -- sh -c 'cargo build && exec ./target/debug/altitude'

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
