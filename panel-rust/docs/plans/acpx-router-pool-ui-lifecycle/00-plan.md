# ACPX router, pool, and UI lifecycle

## Scope

Fix the remaining ACP client-capability schema mismatch and make session-pool
lifecycle explicit across panel initialization, default-agent selection, and
project changes without blocking the Slint UI thread.

## Findings carried into the plan

- `acpx/acpx-core/src/router.rs` now sends `initialize.clientCapabilities.terminal`
  as a boolean in the shared handshake path. ACP v1 requires that scalar;
  `fs` remains the object capability.
- Settings > Agents enable/disable is already fire-and-forget on the bridge
  Tokio runtime.
- Settings catalog discovery is already backgrounded through
  `request_gateway_catalog_refresh`; the frame path reads a cache.
- `AgentBridge::list_agents` still exposes a synchronous `runtime.block_on`
  API and must not be used by UI code.
- Pools are keyed by `(project cwd, gateway URL)` and now expose a non-leasing
  prewarm entry point for the selected default agent.
- A project switch refreshes old project pools: idle entries are evicted and
  active leases become stale so their owner drops them after the current turn.
- Bridge events already use the normal path: background thread actor -> bridge
  event queue -> frame poll -> reducer -> thread view.
- Existing leases may be active during a project change. They must be allowed
  to finish or be released by their owning thread; project switching must not
  force-close an in-flight turn.

## Design decisions

1. Keep ACP capability construction schema-correct at one shared router helper;
   add a regression test that rejects object-shaped `terminal`.
2. Treat pool disposal and lease release as separate operations: invalidate or
   mark old-project idle entries immediately, but defer active-entry removal
   until the owner finishes/releases the lease.
3. Make project switching generation-aware so a late attach cannot return an
   old-project lease into the new project's pool or thread view.
4. Initialize the default enabled agent's pool lazily but proactively on the
   background runtime after the panel has a project/cwd and a resolved gateway;
   first thread attach may still consume the warmed lease.
5. Preserve the existing event queue and thread-view reducer path; only move
   work behind it, never call RPCs or wait on pool/network futures from Slint.

## Phase matrix

### Phase 1 — Router capability correction (Tier 1) — COMPLETE

Change the `initialize` client capability payload so `terminal` is a plain
boolean in every path, while preserving the `fs` object shape. Add focused
serialization/router tests and run the existing ACP core authentication and
library suites.

Verification: serialized initialize params contain `terminal: true/false`,
never `{...}`, and the focused plus full `acpx-core` tests pass.

### Phase 2 — Lease and pool lifecycle model (Tier 2) — COMPLETE

Add explicit project/generation ownership to pool leases. On project change,
invalidate old idle entries and mark active entries stale without interrupting
their current turn. On release, stale active entries are discarded rather
than returned to the idle pool. Add tests for idle eviction, active-turn
release, double release, and a late release after project switch.

Verification: old-project sessions cannot be acquired by a new-project key;
active sessions finish safely; stale leases never become idle.

### Phase 3 — Project-switch handoff (Tier 2) — COMPLETE

Wire `ProjectPathChanged`, project close, untitled creation, and Save-As
handoff into the pool lifecycle. Recompute the new project pool from the
derived ACP cwd, retain per-thread project ownership, and ensure background
attach tasks re-snapshot project identity before acquiring. Release or
invalidate the old thread's lease through the existing actor command path.

Verification: open A -> switch B -> switch A -> close produces no cross-project
session reuse, no stale thread-view event mutation, and no forced cancellation
of an in-flight turn.

### Phase 4 — Default enabled-agent pool initialization (Tier 2) — COMPLETE

After the default enabled agent and gateway are resolved, schedule bounded
pool warmup on the bridge runtime for the active project/cwd. Do not create
sessions for every catalog entry; warm only the selected default agent and
only when a valid project/cwd exists. First attach should consume the warmed
lease when available and fall back to normal acquire when not.

Verification: panel init/project open shows no UI wait; the first attach logs
warm-hit versus cold-open; changing the default agent starts a new keyed warmup
without sharing the previous agent's session.

### Phase 5 — UI-thread audit and cleanup (Tier 3) — COMPLETE

Make the cached/background catalog API the only Settings discovery path.
The UI-reachable profile and MCP tool-preference effects now use bridge-runtime
async methods; legacy synchronous wrappers remain only for compatibility and
tests. Keep completion/errors flowing through the existing queued event path.

Verification: static search shows no UI callback/frame path invoking blocking
RPC APIs; a slow/unreachable gateway leaves the UI responsive and eventually
surfaces a queued error or connecting state.

### Phase 6 — Review gate (review_gate) — COMPLETE

Run the Rust audit over router, pool, bridge, project lifecycle, and actor
changes. Check lock scopes, timeout coverage, stale-task cleanup, and that no
sync mutex guard crosses an await.

### Phase 7 — Verification gate (verification_gate) — COMPLETE

Run implementation verification against this plan and record every
misalignment. Do not close the plan while unresolved misalignments remain.

### Phase 8 — Runtime gate (runtime_gate) — COMPLETE

Exercise the host/UI matrix: Settings discovery with slow gateway, enable
agent, open project A, first attach, switch to B during idle, switch during an
active turn, return to A, and close project. Confirm thread-view notifications
remain correctly attributed.

Runtime evidence: the Slint MCP live suite ran against the compiled `snapflow`
host, Xvfb, and a real `acpx-server` process; four active retained-view,
popup/tool-state, thread-isolation, and switch-during-stream scenarios passed.
The host MCP matrix additionally passed pool switch/session routing, send-now
active-turn steering, immediate send after new-thread attach, and rename/persist
through the real compiled host. The optional ambient billed-adapter smoke
remains environment-dependent and is not part of the default gate.

## Open risks

- Retaining old pools may be required for background sessions; disposal must
  not silently delete recoverable remote sessions.
- Warming a Codex ACP session invokes remote `session/new`/thread creation and
  should be bounded and limited to the default agent to avoid unnecessary
  backend load.
- A project switch during an active turn needs a visible transition state if
  the owning thread remains visible while its old lease drains.
