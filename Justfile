# Rebuild and restart the TUI when active Rust sources change.
dev:
    watchexec --restart --stop-signal SIGTERM --stop-timeout 2s --wrap-process=none --watch src --watch Cargo.toml --watch Cargo.lock --shell=none -- sh -c 'cargo build && exec ./target/debug/altitude'
