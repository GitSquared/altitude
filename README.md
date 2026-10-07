# altitude

**altitude is an opinionated tasks orchestrator that elevates your agentic building experience.**

it connects to your existing coding harness, task tracker, version control, and coordinates agentic models to abstract away the unnecessary details and let you focus on what needs to be built.

it is for you if:
- you care more about what you're building than the puzzles to solve on the way
- you want to draw, talk, scribble and visually review work
- you want to scale & automate to deliver better and stronger

it might not be a good fit if:
- you often fine-tune models and reasoning effort per task
- you keep a close eye on the code and have strong opinions on how it should be written
- you pair with agents more than you delegate to them

altitude comes from my own frustrations with scaling up agentic engineering work, and the belief that agents should empower and accelerate the execution of human intent, rather than steal our attention away.

## Development

The active implementation is a Rust binary at the repository root.

```sh
cargo run
```

For automatic rebuilds and restarts on save (macOS):

```sh
brew install just watchexec
just dev
```

This restarts the app and resets its in-memory state. Press `q` to close the TUI; the watcher stays ready for another save. Press Ctrl-C after closing the TUI to stop the watcher.
Build errors appear in the terminal and the next save retries the build.
On restart, Watchexec sends SIGTERM and waits for the app to restore the terminal and exit.

Check changes with `cargo fmt --check`, `cargo check`, and `cargo clippy -- -D warnings`.


## License

[Business Source License 1.1](LICENSE) — source available, non-commercial self-hosting permitted. Converts to MIT on 2050-01-01. Commercial use requires a license from the author.

---

###### © 2026 Gabriel Saillard <gabriel@saillard.dev>
