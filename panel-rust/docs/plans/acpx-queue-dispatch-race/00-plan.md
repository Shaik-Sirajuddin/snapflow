# ACPX queue dispatch reservation race

## Problem

`spawn_queue_dispatcher` checks `session_is_in_flight`, then awaits the durable
queue claim before sending a synthetic `session/prompt`. A normal client
prompt can start between those operations, so the queued prompt can be
claimed and reach the backend ahead of the normal prompt. Turn completion
then re-arms the dispatcher, making the ordering failure look like an
unexpected auto-forward at the end of a turn.

## Fix

Add a per-session server-side dispatch reservation shared by normal prompts
and queue dispatch. A dispatcher must reserve the session before claiming a
queue entry; normal `session/prompt` dispatch must acquire the same reservation
before marking the session in flight. Release it only after the prompt call has
finished. Queue claims are released if dispatch cannot acquire/complete, and
pause state remains authoritative in `QueueStore::take_next`.

## Verification matrix

- Existing FIFO queue auto-dispatch test remains green.
- New concurrent race test proves a normal prompt cannot be overtaken by a
  queue-dispatched prompt.
- Queue pause still prevents auto-forward.
- ACPX server/client integration smoke runs against the built local binaries;
  if the installed client/server entrypoints are unavailable, record that as a
  runtime limitation rather than weakening unit coverage.

## Follow-up audit findings

The reservation fix closes the normal-prompt versus queue-dispatch TOCTOU, but
the surrounding paths expose separate ordering/reliability gaps:

1. `dispatch_session_steer_shared` publishes `dispatched` and `completed`
   immediately after spawning the dispatcher, before the backend claim or
   response. It also ignores cancellation failure. A client can therefore
   render a failed/paused steer as completed.
2. `session/close`, `session/resume`, and `session/load` do not share the new
   prompt reservation. They can overlap an active prompt and race registry,
   backend, or transcript state. The non-shared `Router::dispatch_proxied`
   path also lacks the reservation, even though provisioning/tests can call it
   directly.
3. `QueueStore::recover_inflight` deliberately requeues any claim left by a
   crash. This remains an at-least-once boundary: if the backend accepted the
   prompt before the crash but `complete` was not written, restart can repeat
   the turn. ACPX now preserves the stable queue entry/idempotency key on every
   retry, but exact once-only recovery requires backend-side deduplication and
   cannot be guaranteed by the gateway after a process crash.
4. WebSocket queue forwarders only log and continue after
   `broadcast::RecvError::Lagged`; they do not request a fresh queue snapshot.
   A client can permanently miss `sent_prompt`/`completed` deltas and retain a
   stale local queue until it reconnects and explicitly resubscribes.

These are distinct follow-ups, not evidence that the inspected Nitro session
lost its first message: that session's first queue entry is present in the
server transcript and has a durable claim/complete pair.
