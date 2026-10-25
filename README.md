# ⛰  altitude

**altitude helps you get shit done with agents.**

i care more about what gets built and the decisions made along the way, than how a very verbose harness and agent cares to narrate me its every side-quest. i already have both team and personal task trackers and don't want my agent sessions list to become a third distinct one to manage. i parallelize my work massively and need agents to understand my attention is a precious resource that needs to be earned, not a direct message channel they can take as granted. i don't trust LLMs about unsupervised access to a shell on my home machine.

so i built **altitude**. it:
- focuses on tasks and decisions, not chat logs
- uses strong microsandbox primitives for agents isolation & host machine flexibility
- drives agents to be more autonomous using a set of opinionated rules and organize attention requests in rationalized "signals"
- plugs in bidirectionally with your existing tools (coding harness, version control, task tracker) rather than replacing them
- supports "remotes" deployment for letting your task run even when your primary device isn't awake

## install

on macOS (apple silicon), via Homebrew:

```sh
brew tap gitsquared/altitude https://github.com/gitsquared/altitude
brew install gitsquared/altitude/altitude
```

on Linux (the host needs KVM access):

```sh
curl -fsSL https://raw.githubusercontent.com/gitsquared/altitude/main/install.sh | sh
```

the installer verifies the release checksum and puts the binary in `~/.local/bin`.
set `ALTITUDE_VERSION` to pin a release, or `ALTITUDE_INSTALL_DIR` to install
somewhere else.

## development

altitude is built as a single Rust binary.

```sh
cargo run
```

for automatic rebuilds and restarts on save (macOS):

```sh
brew install just watchexec
just dev
```

check changes with `cargo fmt --check`, `cargo check`, and `cargo clippy -- -D warnings`.


## license

[Business Source License 1.1](LICENSE) — source available, non-commercial self-hosting permitted. converts to MIT on 2050-01-01. commercial use requires a license from the author.

---

###### © 2026 Gabriel Saillard <gabriel@saillard.dev>
