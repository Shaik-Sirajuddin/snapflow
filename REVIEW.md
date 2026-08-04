# Review: `thread-stream-persistence` worktree vs `main`

> Status update (2026-08-03): Claude re-verified the current implementation.
> Findings #1 and #2 below are historical and are superseded by the claim/release
> retry and recovery changes now in `queue.rs`/`router.rs`; retain only the
> narrower recovery-policy caveat. The active protocol findings are listed in
> the Protocol-alignment section near the end of this file.

Branch: `worktree-thread-stream-persistence`, merge-base `6e3791f6`, 33 commits ahead,
42 files changed (+4569/-131). Objective (from commit history): move ACPX's session
queue and transcript persistence server-side (durable JSONL, FIFO auto-dispatch,
restart recovery, reconnect replay) and wire the panel UI to treat the server as
the source of truth instead of a purely client-local queue.

Plus one uncommitted change in `panel-rust/src/dispatch.rs`.

## Findings, most severe first

### 1. A queue-dispatch failure permanently drops the user's prompt (silent data loss)

`acpx/acpx-core/src/persistence/queue.rs::QueueStore::take_next` (lines ~234-279)
removes the head item from the durable queue **and writes a `Cancel` record marking
it consumed** before the item is actually sent:

```rust
let item = queue.remove(0);
...
append_record(&path, &QueueRecord { operation: QueueOperation::Cancel, ... })?;
...
Ok(Some((item, queue, paused)))
```

The caller, `spawn_queue_dispatcher` (`router.rs:7403`), then tries to deliver it:

```rust
if let Err(error) = dispatch_proxied_shared(&router, &tenant_id, request).await {
    tracing::warn!(%session_id, %error, "queued prompt failed");
    break;
}
```

On failure it only logs a `tracing::warn!` and breaks the loop. Nothing re-enqueues
the item, nothing surfaces an error to the client, and the JSONL log has already
recorded it as cancelled — so replay after this point (including after a server
restart) never reproduces it. Any transient failure of `dispatch_proxied_shared`
(backend process momentarily busy/crashed, a validation error, `session_is_in_flight`
races, etc.) silently and irrecoverably deletes a queued prompt's text with zero
user-facing signal. This is the loss scenario worth treating as a blocker: it directly
contradicts the "durable FIFO queue" premise the whole persistence layer exists for.

**Fix direction**: don't commit the `Cancel`/consumed record until dispatch succeeds
(e.g. claim-and-lease the head item, or only append `Cancel` after
`dispatch_proxied_shared` returns `Ok`), and on failure either requeue at the front
or surface a queue-item error state the client can retry/show.

### 2. Dispatcher isn't resumed automatically after a server restart or after it breaks

`spawn_queue_dispatcher` is only spawned from `dispatch_queue_mutation_shared` on
`Enqueue` / `SendNow` / `Resume` (`router.rs:7375-7399`), and from one other call
site around `router.rs:7948`. `recover_open_sessions_shared` (`router.rs:6853`),
which runs at daemon startup to reattach live backend sessions, never calls
`spawn_queue_dispatcher`. `dispatch_session_stream_subscribe_shared`
(`router.rs:7311`) — the reconnect/subscribe path — only returns a queue snapshot,
it doesn't kick the dispatcher either.

So: a session with a non-empty, non-paused persisted queue at the moment the
acpx-server process restarts (or at the moment `spawn_queue_dispatcher`'s loop
`break`s on any error per finding #1) sits stalled indefinitely until the client
happens to send a fresh `Enqueue`/`SendNow`/`Resume` for that exact session. That
next mutation does drain the whole backlog (FIFO, since `take_next` isn't
session-scoped to "new items only"), so it's self-healing *if* the user acts again,
but a queued message sent right before a crash/restart can sit invisibly stuck
with no automatic recovery and no user-visible "stalled" indication.

**Fix direction**: have `recover_open_sessions_shared` (or an explicit startup
sweep) call `spawn_queue_dispatcher` for every recovered/live session with a
non-empty unpaused queue.

### 3. Unbounded append-only queue log, full replay on every operation

`QueueStore::mutate`/`snapshot`/`take_next` (`queue.rs`) all call `read_records`
which `read_to_string`s and re-parses the **entire** `<session>.queue.jsonl` file
on every single call, and `append_record` never truncates/compacts — every
enqueue/cancel/send-now/dispatch adds a line forever. For a long-lived session with
normal chat usage (many turns × queue churn) this file grows without bound and
every subsequent queue op gets linearly slower (full re-parse) and does a blocking
`sync_data()` fsync per line. Not a crash, but a real "no production style" gap:
there's no compaction, no size cap, no rotation. Worth at least a periodic compact
that rewrites the replayed (already-collapsed) state as a fresh base, analogous to
whatever `transcripts.rs` does (check if it has the same issue — a quick look
suggests `transcripts.rs` has similar unbounded-append shape).

### 4. `dispatch.rs` debug-assert removal: masking or genuinely obsolete? (uncommitted change)

The uncommitted working-tree diff in `panel-rust/src/dispatch.rs::dispatch_compose_send`
deletes the debug-only invariant check that the emitted `SendPrompt` effect's
`thread_id` matches the thread the user actually clicked send on
(`expected_thread_id`, captured via `panel.real_index(filtered_idx)` before the
reducer runs), replacing it with a comment claiming deferred session attachment
can rebind the selection mid-reducer.

Tracing the actual call path (`lib.rs:2429` `on_send_requested` →
`dispatch_compose_send_maybe_attach` → `attach_deferred_thread` → `dispatch_compose_send`
→ `update_compose`'s `idx = selected_real_index(model)` using `model.selected_thread`):
`attach_deferred_thread` (`agent_bridge.rs:3665`) only mutates the *bridge's* own
`self.slots`, then `spawn_background_attachment`s a task on the tokio runtime — it
does not synchronously touch `panel.model`. Everything between reading `filtered_idx`
off the live Slint property and running the reducer is single-threaded, synchronous
Rust with no yield point, so I could not find where model.selected_thread would
actually diverge from filtered_idx "while the reducer is running," as the comment
claims. That doesn't mean the crash reported from VNC builds was fake, but the fix as
committed simply deletes the check rather than narrowing it to whatever the real
divergence source was — worth either (a) finding the actual repro and asserting a
weaker-but-still-meaningful invariant, or (b) adding a regression test that pins down
the scenario the removed assert used to guard against, so a real thread-target
mismatch (sending to the wrong thread) isn't silently possible in release builds
without any test coverage at all going forward. Currently this diff has no test
supporting the removal, which is the real gap — an invariant got deleted, not proven
unnecessary.

## Non-issues (checked, looked fine)

- No new `.unwrap()`/`.expect()`/`panic!` were added in non-test code across
  `queue.rs`, `transcripts.rs`, `router.rs`, `ws.rs`, `config.rs`, `thread_actor.rs`,
  `send_queue.rs`, `update.rs`, `agent_bridge.rs` (grepped the diff hunks).
- `QueueStore::path` rejects `..`, `/`, `\`, empty session ids before touching the
  filesystem — reasonable path-traversal guard, reused from `TranscriptStore`.
- `append_record` does `sync_data()` per write — correct for durability, just the
  performance cost noted in #3.
- Idempotency-key dedup in `mutate`/`replay_records` looks correct for the FIFO +
  dedupe + steer/send-now-reorder logic; the four `queue.rs` unit tests (FIFO,
  restart-replay, send-now reorder, cancel-absorbs-entry) meaningfully exercise it.

## Summary

Two real correctness gaps (findings #1 and #2) directly threaten the "durable,
no-loss FIFO queue" objective this branch exists to deliver — #1 in particular is a
straightforward silent-data-loss bug that should block merge until the
claim-before-success ordering in `take_next` is fixed. #3 is a scalability/production-
hardening gap, not a crash. #4 is a working-tree-only change that trades a possibly-
overzealous panic for zero invariant checking at all, without a repro or regression
test proving the removal is safe.

## Protocol-alignment review (2026-08-03)

- `session/steer` origin filtering is now implemented end-to-end. Dedicated
  `acpx/session/steer` lifecycle events (`queued`, `dispatched`, `completed`)
  are emitted separately from queue deltas; the transport-private origin marker
  suppresses the initiating connection, and the real WS test verifies it.
- Agent-request resolution fanout does carry server-managed client IDs and
  excludes the winner from `acpx/agent_resolution`; this is covered by relay and
  process-matrix tests.
- `panel-rust` protocol handling is now wired: queue delta projection,
  server-owned send-now mutation, permission-profile RPC wrappers, and
  `acpx/agent_resolution` loser cleanup are implemented. See
  `panel-rust/REVIEW.md` for the Slint fixture verification (21/21 passed).

## Final verification update (2026-08-03)

- Dedicated steer parser/consumer tests: 22 passed.
- Real ACPX process matrix, including dedicated steer origin filtering: 5 passed.
- Session-attached `agent_full_access` is now applied before the first demux
  policy is captured; the real wire test exercises terminal create/output under
  that policy and passes.
- An actual opt-in wire test now exists at
  `acpx/acpx-server/tests/real_agent_session_sync_e2e_test.rs`; it launches
  ACPX, drives `session/new`/`session/prompt`/`session/load`/`acpx/sessions/sync`,
  asserts zero diff, closes the session, and cleans up its child/database.
  It requires an externally configured ACP adapter command and credentials. The
  bundled ACP-compliant mock also passes the same terminal/tool/message and
  zero-diff sync gate.
- Real-agent test cleanup now reaps the child process before removing its
  SQLite database/keyring, and uses null stdio handles so verbose adapters
  cannot deadlock on undrained pipes.
- Profile relay tests use per-test SQLite paths; the server transcript now
  records the originating user prompt before streamed updates.
