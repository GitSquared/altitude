# Altitude remote: reviewed decisions

Status: authoritative record for the remote/server work. Agreed 2026-10-03 in a
review of `server-mode-ux.md`. Where the two disagree, this document wins; the
superseded points are listed explicitly at the end.

`server-mode-ux.md` remains useful as the original sketch and for its diagrams,
but it was written without review and several of its choices have been replaced.

## How to read this

Each decision below records the question, the options that were on the table, the
answer, and the reasoning. The reasoning matters more than the answer: it is what
lets a later change be judged as a refinement or a reversal.

Two rules held throughout:

- **Altitude is a tool, not a service.** It holds as little as possible on the
  operator's behalf, it never acts on their behalf where a human judgment is
  implied, and it is unhooked as easily as it is hooked up. The product image is
  a trailer you attach to a car, or a bit you screw into a drill.
- **Altitude is additive.** Local autopilot → version control → remote node →
  task tracker. Each step is optional and each one is useful on its own. Nothing
  later may be required to make something earlier work.

---

## Context: what the review found

Three bodies of evidence shaped the decisions.

### The current Rust implementation already has the shape

- The capsule/identity split exists. `PersistentDirectory` in
  `src/isolation/environment.rs` bind-mounts credentials and settings from
  Altitude's own profile directories, while conversation state
  (`projects`, `sessions`) is redirected to the task's disk and travels with its
  snapshot. Conversations travel; auth does not.
- Attention semantics exist: `TaskOperatorSignalKind` (update, decision, review,
  failure), `TaskOperatorAttentionLevel`, priorities, read/resolved state, and a
  store that refuses to answer a signal while the task is still running.
- `TaskExecutionMode` (autopilot/operator) is already the *driver* axis, durably
  recorded. Nothing records the *hosting* axis.
- Environments travel as a recipe (`name`, `setup_command`, `harness`) recorded in
  the task's history; each node materialises its own base snapshot from it.

### External reality, as of this review

- Linear's Agents API is in **Developer Preview**. Delegation sets `delegate`,
  not `assignee`; the human keeps ownership. Session states (`pending`, `active`,
  `error`, `awaitingInput`, `complete`, `stale`) are managed by Linear from the
  activities an agent emits. `created` requires an activity within 10 seconds or
  the session shows as unresponsive; sessions go stale after 30 minutes of
  silence; webhooks must be answered within 5 seconds and are retried at 1 minute,
  1 hour and 6 hours, at most 3 times, after which Linear may disable the webhook.
- Elicitation replies arrive as free-text `prompt` activities, including when the
  user picks one of a `select` signal's options. There is no typed verdict on the
  wire. Linear's own guidance is to interpret the reply with a model.
- Linear signals exist and are directly useful: `stop` (raised by a native "Send
  stop request" gesture), `auth` (an elicitation carrying an account-linking URL),
  `select` (options on an elicitation). `issueRepositorySuggestions` ranks
  repository candidates. `guidance` rules arrive in `promptContext`.
- **Linear already ships this journey first-party.** Coding sessions delegate an
  issue into a Linear-managed sandbox running Claude Code or Codex, draft a PR,
  and review it in a Reviews tab with diffs and verification artifacts, with
  per-repository coding environments and AI-credit billing.
- **Anthropic ships the other half.** Claude Code Remote Control serves a local
  session to claude.ai/code and mobile with server mode, multi-session and
  worktree spawning; cloud sessions and self-hosted environments cover hosted
  execution. Caveats that leave room: Remote Control requires a subscription
  login (API keys are not supported), stores transcripts on Anthropic's servers,
  is unavailable behind gateways/Bedrock/Foundry and for ZDR organisations, and
  is Claude-only.
- Microsandbox snapshots are **disk-only by default** (memory and devices require
  an explicit full capture), restore as a **cold boot**, record no integrity
  unless asked, resolve a **pinned image digest**, and can be archived as
  `.tar.zst` with the image included so they boot offline elsewhere.
- Iroh is at 1.x: dial by public key, hole punching with relay fallback, QUIC,
  verified streaming for blobs. `AGENTS.md` already names it as a networking
  building block.

### The Straylight prototype, as an idea catalogue

Useful ideas were taken from it; it is not a reference implementation and was
reported to be fragile in practice (agents failing to raise QA correctly).

- `attention.ts` models three kinds: `signal` (nonblocking), `steering` (blocking
  decision), `qa` (blocking approval of checked work). **QA cannot be raised
  without evidence** — validation rejects an empty evidence list. QA has fixed
  options, and the reply is classified from text (`isQaApproval`).
- Its prompt states the contract plainly: the engineer owns completion, the agent
  never declares work complete, and apparently finished work is handed to QA.
- `capacity.ts` implements adaptive slots: sample every 10 s over a 10-minute
  window, take p75 of CPU and memory, target 75 %/80 %, grow by one only under
  demand, shrink on pressure, floor of one.
- `capsule/transfer.ts` models ownership as an epoch plus a lease with explicit
  transfer phases and a fail-stopped terminal for an ambiguous activation
  acknowledgement.

---

## The model

1. **Two axes, never fused.** *Driver* (autopilot ⇄ operator) is fluid and cheap.
   *Hosting* (which machine holds the capsule) changes only by an explicit move.
2. **Control means pull.** There is no remote terminal attach. The two remote
   surfaces are signals and pull/push.
3. **Ownership follows hosting.** The node is home; you send the task back when
   you are done.
4. **A move is a move.** Source relinquishes liveness before the payload leaves;
   the destination becomes live only once it holds everything.
5. **Identity originates on the client** as a keypair; a node holds a delegated
   replica. One push carries identity, configuration and capabilities.
6. **Permission is scoping; authority is liveness.** Tasks belong to an identity.
   Authority is only a monotonic epoch plus which node is live.
7. **Isolation is per identity**, in one instance, with per-identity storage roots.
8. **The tracker is an adapter, not a state store.** Stable external identity is
   portable with the task; projection state is node-local and rebuildable.
9. **Altitude owns the signal contract.** Good operator-facing signals are
   defined by Altitude, which coerces the agent into producing them and conveys
   the artifacts in both directions.
10. **Disposal is a human-confirmed acceptance signal.** Nothing disposes a
    capsule automatically.
11. **Node and client are one binary in two roles.** Not two products.

---

## Decisions

### Q1. Where does ownership live — what is it?

**Options:** one concept covering both "who holds the capsule" and "who is
driving"; or two.

**Decision:** two axes, never fused.

**Why:** `TaskExecutionMode` already records the driver durably and is correct.
Folding hosting into it would make every existing match on execution mode lie.
The driver changes in milliseconds and must never require a snapshot; hosting
changes in minutes and must never be implied by taking control. (The storage
question this originally asked was answered in Q12.)

### Q2. Does taking control move the capsule?

**Options:** remote attach by default, with transfer as an escape hatch; or
control implies pulling; or policy per case.

**Decision:** control implies pulling. Remote terminal attach is not a product
surface.

**Why:** steering through signals covers the overwhelming majority of
interactions, from the TUI or from Linear. Taking the raw harness is rare and
already exceptional, so paying a move for it is justified. This removes an entire
mechanism — remote terminal streaming — from the protocol, leaving signals and
transfer as the only two remote surfaces.

The cost of a move was initially overstated as "minutes". It is seconds for a
modest repository: snapshots capture only the writable layer and compress well.
The real costs are a cold boot, the destination needing its own harness auth, an
integrity pass, and the window where nothing is running.

### Q3. After overriding locally, where does the capsule live?

**Options:** it returns to the node; it stays on the desk until pushed; or per
task.

**Decision:** it returns. Ownership follows hosting, and the node is home.

**Why:** "you don't keep clutter on your desk". One obvious home per task keeps
the tracker's view honest. Ownership tracking where the capsule is avoids the
question "who owns a capsule nobody is running", which is the question that
creates two live owners.

### Q4. What moves when you pull?

**Options:** the desk becomes a node and takes history, capsule and authority; or
the desk is only a runtime host while the origin keeps the log.

**Decision:** the desk becomes a node. Full transfer, history included. The
tracker binding follows the capsule: pulling detaches it from tracker sync,
sending it back reattaches.

**Why:** a runtime-host model stops working the moment the origin node is
unreachable, which defeats "just like developing locally". Full transfer means
task history must be portable — the prototype already prototyped this shape as
portable task state.

Consequence: while a task is on a desk it is detached from the tracker. This must
be visible rather than silent — the issue should say the task is pulled and where
it is.

### Q5. Is harness auth node-level or pushed from the client?

**Options:** each node logs into each harness itself; or the client pushes a
scoped capability so a node can run work it was never logged into.

**Decision:** pushed capabilities. A node is a blind runner that accepts work on
behalf of clients, and a shared node is multi-principal.

**Why:** the target is a node that can be deployed for a team and bound to a
shared tracker, where several engineers push their own tasks and keep clean
isolation between their tasks and harness identities. Per-principal pushed
credentials are what make that possible without turning the node into a shared
login.

### Q6. Where does a principal come from?

**Options:** node-local identities mapped from tracker users; tracker users as the
principal; client credentials as the principal.

**Decision:** identity originates on the client, as a keypair born in the
operator's own install. Pushing to a node ships a delegated replica of who you
are, how to run your agents, and the capabilities to do it. A tracker user or a
paired machine is a *credential for* a principal, never the principal itself.

**Why:** Altitude is first and foremost a personal tool installed because it fits
the way someone works. Giving the keys to a valet is a deliberate act of the
owner. It also keeps the "tracker is an adapter" rule intact — an external system
may point at a principal, never be one. Pushing identity, configuration and auth
is one mechanism, not three.

### Q7. How long does a delegated identity stay valid on a node?

**Options:** until revoked; a mandate window; a short lease refreshed by the
client.

**Decision:** until revoked. No mandate clock. Revocation stops new turns
immediately and lets an in-flight turn finish.

**Why:** a lease contradicts the point — a node you cannot reach would stop
driving your tasks. A mandate window adds a renewal chore and a silent-stop
failure mode, and silently halting work is worse than a key that lives slightly
too long. In practice the harness credential has its own expiry, and the repair
loop is pull → re-authenticate → push back. Altitude does not manage credential
lifetime; it inherits it.

Consequence: an expired upstream credential must surface as a specific, repairable
state pointing at that loop, not as a generic failure.

### Q8. How does a client reach its node?

**Options:** direct HTTPS for everything; split paths where signals ride the
tracker; or Iroh for everything.

**Decision:** Iroh. Node identity is its keypair; the client↔node path needs no
inbound port and no public URL. Tracker ingress is a separate, later hook.

**Why:** it keeps "delegate to a remote" independent of "be publicly reachable
from the internet for a tracker". Making step two of the journey depend on step
three is friction too early. Iroh is already a named building block in
`AGENTS.md`, gives dial-by-public-key, hole punching with relay fallback, and
verified blob transfer — which is exactly the shape a capsule move needs. A node
can therefore be deployed with no ingress at all until a tracker is hooked up.

### Q9. How isolated are two operators on one shared node?

**Options:** one instance with principal-tagged rows; one instance with identity
as a first-class dimension; or a host of independent instances.

**Decision:** (b) — one instance, identity as a first-class dimension: N
long-lived identity objects, each with its own profiles, snapshot store and
sandbox home. A task records the identity it was delegated under.

**Why:** it gives the requested isolation without building a supervisor, and it
does not foreclose (c). The prototype already had this object (a long-lived
identity capsule with its own volume mounted at `/identity`), so this is the same
thing with N of them.

Noted for later: (c) may turn out to be both stronger and *simpler*, because it
does not alter the base tool at all — an altitude instance is fully described by
three directories, and `Config` already accepts `ALTITUDE_DATA_DIR`,
`ALTITUDE_CONFIG_DIR` and `ALTITUDE_CACHE_DIR` overrides. A host of instances is
therefore N service/event-loop triples over N roots plus a front door, with no
schema change. The cheap hedge is to keep per-identity storage roots from day one.

### Q10. Is approval in the tracker Altitude's acceptance?

**Options:** yes; or approval is only an input requiring a second confirmation;
or per binding.

**Decision:** yes — Linear's approval *is* the human judgment, recorded by
Altitude as an acceptance event, gated on the actor mapping to the principal.

**Why:** it is what makes "conduct almost the whole task from Linear" true, and it
is what the original thoughtpiece's own product test asserts. The gate matters:
actions from a tracker actor are commands only if that actor maps to the
principal. An unmapped teammate's mention is context, exactly like an ordinary
comment. This makes the "only explicit addressing is steering" rule stronger —
two filters, both failing closed.

### Q11. What counts as review evidence?

**Options:** whatever the agent reports; artifacts the agent is prompted to
produce; or evidence the node captures independently.

**Decision:** Altitude owns the signal contract. It defines what a good
operator-facing signal looks like, which coerces the agent into producing the
right artifacts; Altitude's job is then to convey them to the operator and back.

**Why:** the contract is the mechanism, not a separate evidence pipeline. The
prototype encoded this well: QA cannot be raised with an empty evidence list, and
QA has fixed options. Node-side capture is not needed to make the contract real,
though a terminal recording is nearly free because the node already sees every
byte and currently discards it.

### Q12. Is authority event-sourced or a table?

**Options:** history events; a lock table plus audit events; a table alone.

**Decision:** history events, and a much smaller concept than first proposed.

**Why:** every write already runs inside `BEGIN IMMEDIATE`, which holds SQLite's
write lock, so reading the current epoch inside the transaction and rejecting a
stale one *is* a compare-and-set. No second source of truth is needed.

The concept shrank because of Q6 and Q9: with tasks scoped to an identity and
identities isolated, cross-operator permission questions disappear. Authority is
not "who is allowed" — that is scoping. It is "which copy is live", which is
needed because a move creates a second physical location for the same task, and
the same operator is on both sides of it. Concretely it is a monotonic epoch plus
which node is live. No leases, no expiry, no holder principals.

**Move ordering rule:** the source relinquishes liveness before the payload
leaves, and the destination becomes live only once it holds everything. One
ambiguous moment remains — the final acknowledgement — and it resolves to
fail-stopped: nobody runs, the operator is asked. Silence must never become
autonomous execution.

### Q13. Cursors and idempotency: one substrate or per surface?

**Options:** one substrate using the event log's sequence as cursor and a unique
dedupe key per producer; per-surface ledgers; or nothing yet.

**Decision:** one substrate. The event log's `sequence` is the client's cursor,
and every producer supplies a stable key that a unique index dedupes.

**Why:** a replay becomes a no-op and a reconnect becomes "everything after N",
from one mechanism instead of three. Note that `read_task.sql` already computes
`last_event_sequence` and it is silently dropped, because `TaskFacts` has no field
for it.

**Where dedupe is reserved:** operations that *start or move* work — delegate,
stop, pull, push. Operations that only *record* should instead converge: "this
signal is resolved with this answer" is safe to repeat, and a late reply to an
already-resolved signal becomes new context rather than an error. Our own records
never accumulate duplicate entries; unreliable information from external systems
is absorbed at the boundary and converted into clean internal records.

### Q14. What happens when a node is at capacity?

**Options:** queue with a visible reason; oversubscribe; or reject with a signal.

**Decision:** queue, with capacity computed automatically.

**Why:** the per-task CPU and memory values are **ceilings, not pre-allocations** —
most tasks use much less most of the time — so they are a packing lever rather
than a reservation. The prototype's adaptive slots are the reference: p75 over a
10-minute window, grow only under demand, shrink on pressure, floor of one. A
refinement for Altitude: because capsules are fixed-size VMs, also compute a
deterministic reservation ceiling and take the minimum of that and the sampled
limit. Reservation gives a stable ceiling; sampling catches everything else on
the host.

Queueing is nearly free: `Pending` already exists in the status enum and the TUI
already renders a pending state with progress. Visible queueing is also how
resource pressure becomes legible, which is the signal to scale up.

### Q15. What disposes a capsule?

**Options:** nothing automatic; disposal on acceptance; or keep today's behaviour.

**Decision:** acceptance is a distinct signal kind — a request for the operator to
confirm wrapping up. It requires evidence, it is confirmed by a human, and the
confirmation disposes the capsule, its working memory and its files.

**Why:** "the agent thinks it has done all it can — do you confirm we wrap up?" is
a human judgment, not an agent one. The prototype's `qa` kind is the shape to
keep, including its evidence requirement and fixed options. The agent never holds
the delete key.

**This changes current behaviour.** Today `TaskService::complete` calls
`release_resources`, which removes the sandbox *and* the snapshot, and the agent
decides when to report `complete`. After this change, `complete` becomes "raise an
acceptance request and wait", and `Completed` means operator-confirmed.

**Ephemerality.** No archive of old capsules, working directories or full agent
conversations is kept. Context for further work is reconstructed from the
tracker's journal of signals, the pull requests, the merged code and other
artifacts. The capsule is working memory; the operator-facing record is the
durable memory. Note this is a *policy at acceptance*, not a per-turn fact:
Altitude's capsule persists across turns via snapshots, which is what makes
pull-and-inspect possible at all.

### Q16. How much tracker state does Altitude write?

**Options:** minimal workflow-state writes per the tracker's own guidance; mirror
Altitude's states onto the tracker's workflow; or read-only.

**Decision:** minimal workflow-state writes — move the issue to the first
`started` state when work is accepted — and rich use of everything else.

**Why:** Linear's own guidance asks agents to limit workflow-state writes and to
express state through session state and activities; workflow states belong to the
human's planning. Mirroring also fights teammates' own transitions, which the
prototype had already been bitten by. But minimal does not mean austere: comment
threads per steering signal, the ability to discuss in a thread before validating,
document artifacts, and the full session stream with model and logs are all part
of the product. The requirement to hold onto is: **the operator should never need
to reach a shell on the node to find out what is happening.**

---

## Slices

1. **Identity and durable authority.** A task records the identity it was
   delegated under; control gestures become durable and epoch-checked. Fixes two
   latent bugs on the way: a control request in the in-memory `handoffs` map can
   fire at the end of the *next* run, and `suspending` is never cleared, so a task
   can silently stop recording run endings. Observable in today's TUI, no
   networking.
2. **Acceptance as a signal.** `complete` becomes a request; evidence is required;
   confirmation disposes. Observable in today's TUI.
3. **Headless node and push.** Push identity and configuration; queue with
   computed capacity; delegate tasks that keep running with the laptop closed.
4. **Linear.** Intake, the signal contract on Linear's surfaces, acceptance, and
   the "no shell needed" projection. Prerequisite: a small public ingress shim,
   because Linear requires a public HTTPS webhook URL.
5. **Pull and push over Iroh.** A full move, one live owner, working with the
   origin node unreachable.
6. **Team.** N identities on one node, actor→principal mapping.

The original thoughtpiece ordered Linear before transfer. The order above puts
transfer later: the risk of Linear amplifying authority bugs is handled by proving
authority in slice 1, and after that Linear is the daily driver while a move is
the rare operation.

---

## Superseded in `server-mode-ux.md`

- **Slice order** (Linear at 3, transfer at 4) → transfer follows Linear.
- **Workflow-state mapping** ("In Review while awaiting a decision", per-team
  state mapping as a setup decision) → minimal writes, per Q16.
- **"Most pull gestures should first be a remote attach"** → control means pull;
  there is no attach surface, per Q2.
- **"The signal ID, not comment text, links the reply to the paused task"** →
  replies are free text; correlation is by outstanding elicitation, per the
  evidence above.
- **Credentials "are references or capabilities"** → correct, and now decided:
  capabilities are pushed per principal, per Q5.
- **Review approval as a configurable binding** → approval is acceptance, gated on
  actor mapping, per Q10.
- **`Task` may outlive an issue, a conversation, a capsule instance or a server** →
  still true, but the task's own record is now explicitly narrower: decisions and
  signals persist, capsules and transcripts do not, per Q15.
- **Discovery layers, mDNS, node pools** → deferred; explicit pairing over Iroh is
  the whole story for now.
- **No mention of** identity/principal, authority epochs, move ordering, capacity,
  acceptance-as-signal, or Iroh. Those are new here.

## Deferred

- The exact set of tracker affordances used within a task (threads, documents,
  reactions, the session stream) — settled by use, not by argument.
- Capacity tunables, and the per-capsule CPU and memory ceilings themselves. They
  are placeholders chosen arbitrarily.
- Whether the node becomes a host of instances (Q9's option c) once a real team
  needs a harder boundary.
- Tracker ingress: operator-provided URL and reverse proxy versus something else.
- Whether node-side terminal recording is worth persisting, given the bytes are
  already in hand.
- The one place worth reconsidering the "no tests yet" rule: the move state
  machine is pure, subtle, and the kind of thing a test suite is genuinely for.

## Evidence and its limits

The Linear, Claude Code, Microsandbox and Iroh statements above come from reading
their current documentation, the Microsandbox 0.7.3 crate source, and the Iroh
release material. The Straylight statements come from reading that branch. The
implementation statements come from reading this repository.

Nothing in this document has been established by running a node, a transfer, or a
tracker integration. Every "consequence" above is a design inference, not an
observation.
