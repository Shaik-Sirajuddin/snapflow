# ACPX runtime and project-switch recovery

## Problem

Nitro logs show panel-rust repeatedly failing before requests reach acpx-server with `A Tokio 1.x context was found, but it is being shutdown`. Separately, project switching soft-closes an ACPX session, releases its pool lease, but leaves the actor's local session id/attachment state populated. The next call can therefore use a detached session.

## Production design

1. Treat the panel-owned Tokio runtime as the lifetime owner of all gateway tasks. Do not let detached work outlive the bridge runtime or call into a runtime that has begun shutdown.
2. Make ACPX session lifecycle state explicit: `Attached`, `Detaching`, and `Detached`. A background close must clear the local session/lease state before it completes.
3. Serialize project-switch teardown with attachment and prompt dispatch. A detached thread must be reattached through the normal pool path before a prompt is accepted.
4. Keep the shared acpx-server WebSocket/HTTP service alive; only logical session leases are released. Server-side reset is treated as a recoverable transport event.

## Verification matrix

- Runtime shutdown: no gateway call is spawned after bridge runtime shutdown; callers receive a bounded lifecycle error.
- Session detach: background close clears local session state and releases the lease exactly once.
- Project switch: prompt before switch succeeds; switch; prompt after reattachment succeeds and uses `session/resume`/pool attach.
- Transport reset: server remains healthy and client reconnects without duplicate `session/new`.
- Existing panel-rust and ACPX tests remain green.

## Known residual

The Nitro binary is an older running build, so runtime verification must be repeated against the rebuilt local-main binary after the code changes.
