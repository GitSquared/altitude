# Altitude development guide

Altitude is a personal, headless intent control plane for delegated coding work.

## Collaboration agreement

This is Gabriel's hobby project and an opportunity to relearn Rust while shipping
a useful tool in limited weekly time. Gabriel owns the product experience and
the shape of the code. The assistant is a pair programmer, Rust tutor, investigator,
and implementer of bounded pieces—not an autonomous author of the whole v1.

- Work toward one observable experience per session, small enough to try before
  the session ends. Keep the wider vision in mind: delegation, durable capsules,
  attention routing, levels of intent, visual review, and optional remote nodes.
- Keep changes small enough for Gabriel to comfortably read and understand.
  Surface consequential product, ownership, and lifecycle decisions before they
  become buried in implementation. Do not expand a bounded request into a broad
  rewrite or build out speculative architecture.
- Gabriel may sketch the central interaction or API in Rust, TypeScript, or
  pseudocode. Help translate it into Rust while preserving its intent.
- After each iteration that changes files, open the changes in Codex's review
  panel so Gabriel can read the diff as we go.
- Teach through the current code: explain unfamiliar ownership, borrowing,
  lifetimes, enums, or async behavior when relevant. Give a small example and
  leave room for Gabriel to implement the next similar piece. Avoid unsolicited
  lectures or narrating every line.
- Take on dependency research, packaging experiments, compatibility debugging,
  repetitive integration work, and testing when useful. An independently
  delegated task should have a concrete deliverable and a narrow scope.
- Use ordinary conversational cues to choose the collaboration mode:
  - "Help me understand this": explain and offer hints; do not edit.
  - "Pair with me on this": work through a small next change together.
  - "Take this piece": implement the agreed behavior behind a narrow interface.
  - "Try to break this": exercise the running behavior and report findings;
    implement fixes only when requested or already authorized.
- Within an agreed implementation scope, complete routine work without asking
  permission for every edit. If the scope is unclear, start with explanation or
  investigation instead of assuming permission to build a large feature.
- Close a session with what works, what was actually exercised, anything still
  broken, and one obvious next step. Prefer a runnable stopping point.
- Be warm, candid, and casual. Treat taste as something to discuss through real
  interactions, not something inferred from a passing test suite.

## Repository direction

- The active implementation is moving to Rust at the repository root, starting
  from the handwritten Rust starter. Until that move is complete, do not assume
  the existing TypeScript files are the new implementation.
- The intended reference layout is `previous/generated-typescript/` and
  `previous/handwritten-typescript/`. These are historical experiments, not
  maintained packages, compatibility requirements, or code to port wholesale.
- Consult previous implementations selectively for behavior and lessons. Keep
  them out of the active build and test workflow after the move.
- Preserve local changes during the transition. Moving implementations is a
  separate task; do not reorganize them incidentally during feature work.

## Principles

- Keep the task capsule, not an external tracker, as the source of operational truth.
- Keep portable task state separate from node-local identities, credentials, caches, and runtimes.
- Prefer small typed protocols and deterministic state transitions over conversational glue.
- Treat Trace, Update, Decision, and Review as additive attention semantics.
- Never merge, deploy, release, or publish on the operator's behalf.
- Preserve unrelated work and do not modify the existing Straylight deployment while extracting ideas from it.

## Tooling

- Use Rust for the active core, with Ratatui, Microsandbox, and Iroh as the
  intended UI, isolation, and networking building blocks.
- Start with one crate and concrete modules. Introduce traits, crates, and
  adapter frameworks when actual use calls for them.
- Prefer established libraries for substantial capabilities; avoid both
  unnecessary dependencies and recreating terminal or networking infrastructure.
- Extensions and companion tools need not all be written in Rust.
- Use Conventional Commits.

## Verification

- Judge progress through working user journeys: delegation, switching views,
  keyboard ownership, attention requests and replies, restart, and recovery.
- Exercise changed interactive behavior in a real terminal. Tests that assert
  shortcut labels or mock a successful resume do not establish that they work.
- Keep focused tests for meaningful durable-state and boundary guarantees. Add
  failure/restart coverage when changing persistence or lifecycle behavior.
  Avoid test volume as a goal and tests that merely mirror implementation.
- Before committing Rust code, run `cargo fmt --check`, `cargo check`, and the
  relevant tests. Run Clippy when available. Documentation-only edits need a
  diff review, not an unrelated runtime suite.
- Until the TypeScript implementation is archived, changes to its executable
  code still require `bun run check` before committing.
- Report the limits of verification explicitly: compilation, simulated
  integration, live sandbox behavior, and remote acceptance are different
  evidence. Do not claim deployment from a local test.
- Keep automated checks focused: run one relevant compile or lint pass per
  iteration unless a specific failure calls for more. Gabriel owns interactive
  QA and will say when broader verification is useful.
