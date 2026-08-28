# Altitude development guide

Altitude is a personal, headless intent control plane for delegated coding work.

## Principles

- Keep the task capsule, not an external tracker, as the source of operational truth.
- Keep portable task state separate from node-local identities, credentials, caches, and runtimes.
- Prefer small typed protocols and deterministic state transitions over conversational glue.
- Treat Trace, Update, Decision, and Review as additive attention semantics.
- Never merge, deploy, release, or publish on the operator's behalf.
- Preserve unrelated work and do not modify the existing Straylight deployment while extracting ideas from it.

## Tooling

- Use Bun and strict TypeScript.
- Use `bun:sqlite` for local durable state.
- Add dependencies only when the platform cannot provide the capability cleanly.
- Write Bun tests beside stable public behavior under `test/`.
- Use Conventional Commits.

## Verification

Run `bun run check` before committing. Add focused failure and restart tests for every durable state transition.
