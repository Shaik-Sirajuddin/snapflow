# Architecture

## What acpx is

`acpx` is a Rust gateway daemon (`acpx-server`) that sits in front of one
or more ACP (Agent Client Protocol) backend agent processes (Claude Code's
`claude-agent-acp`, `codex-acp`, or any other ACP-speaking binary/npx/uvx
package) and presents one consistent ACP JSON-RPC surface to clients over
stdio, HTTP, or WebSocket -- regardless of which backend agent actually
services a given session, and regardless of which transport a given
client is using.

Two design goals drive most of the non-obvious code shapes in this
workspace, and are worth keeping in mind before reading anything else
here:

1. **Multiplexing without lock contention.** Many clients, over many
   transports, talking to many distinct backend agent processes
   concurrently -- a slow call to backend A must never block a call to
   backend B, and the gateway's own bookkeeping lock must never be held
   across a real (multi-second, real-LLM-latency) I/O round trip.
2. **Transport-independent session state.** A client's transport
   connection (its WS socket, its stdio pipe) is not the thing that owns
   a session or a backend process. Both live in the daemon's own
   in-memory state, so losing/reopening a transport connection is cheap;
   only losing the *daemon process itself* is expensive (see
   "Persistence and restart recovery" below).

## High-level components

```
                    +-----------------------------------------------+
                    |                 acpx-server (daemon)          |
                    |                                                |
  client -- stdio -->  transport::stdio  --\                         |
  client -- HTTP  -->  transport::http   ---+--> acpx_core::Router  |
  client -- WS    -->  transport::ws     --/    (Arc<Mutex<Router>>) |
                    |                              |         |       |
                    |                              |         |       |
                    |                  SessionRegistry   NotificationHub
                    |                  (gw id -> agent,       (live
                    |                   backend session id)   session/update
                    |                              |          fan-out)
                    |                              v                |
                    |                  acpx_conductor::Supervisor    |
                    |                  (HashMap<agent_id,             |
                    |                   SharedBackendProcess>)        |
                    |                              |                 |
                    +------------------------------|-----------------+
                                                   v
                          child process: claude-agent-acp / codex-acp / ...
                          (stdio JSON-RPC, framed newline-delimited)
```

- **`acpx-proto`** -- shared wire types: the JSON-RPC envelope and
  `session/*`/`agent/*` payload shapes, used by both the server side and
  the client SDK so they can never drift apart.
- **`acpx-registry`** -- client for the official ACP adapter registry
  (index lookup + install-runtime helpers for `npx`/`uvx`/`binary`
  distributions).
- **`acpx-conductor`** -- process supervision: spawn/respawn-on-crash
  with backoff, stdio JSON-RPC framing, the per-process lock
  (`SharedBackendProcess`) that lets two different backends' calls run
  fully in parallel.
- **`acpx-core`** -- the gateway's brain: `Router` (dispatch/
  classification), `SessionRegistry`, `Supervisor` ownership, profiles/
  providers/keystore (managed-mode credential resolution), sqlite
  persistence, the live-notification hub, agent auto-detection.
- **`acpx-server`** (binary) -- daemon entrypoint: env-driven config,
  optional startup provisioning file, wires one shared `Router` to the
  stdio, HTTP, and WS transports running concurrently, plus the
  `acpx-selftest` black-box CLI.
- **`acpx-client`** -- Rust client SDK (raw JSON-RPC wrapper + typed
  extension helpers for sessions/prompts/profiles/registry) for
  consumers of the gateway.

## Request lifecycle

Every inbound JSON-RPC request, regardless of transport, is dispatched
through `acpx_core::router::dispatch`/`dispatch_shared`
([`router.rs`](../acpx-core/src/router.rs)), which first classifies the
method (`classify()`) into one of three buckets:

- **`GatewayNative`** -- answered entirely by acpx itself, no backend
  round trip: `initialize`, `authenticate`, `logout`, `agents/list`,
  `agents/status`, `agents/install`, `profiles/list`, `profiles/delete`,
  `mcp_servers/list`, `mcp_servers/delete`, `session/list` (aggregates
  across all live + real-backend-reported sessions).
- **`Hybrid`** -- `session/new` only: acpx resolves `_acpx.profile` (if
  given) to an agent id/provider/key/launch spec, ensures that backend
  process is running, forwards the real `session/new` call to it, then
  registers the returned backend-native session id against a freshly
  minted gateway session id in `SessionRegistry`.
- **`Proxied`** -- everything session-scoped after that
  (`session/prompt`, `session/cancel`, `session/set_mode`,
  `session/load`, `session/resume`, `session/delete`, `fs/*`,
  `terminal/*`): acpx looks up the gateway session id in
  `SessionRegistry`, rewrites it to the backend-native session id in
  place, forwards the request byte-for-byte otherwise, and returns the
  backend's response unchanged (aside from the id rewrite).

`session/cancel` gets its own branch even within `Proxied`: it is
notification-shaped (no reply expected) and must not block on
`read_matching_response`, and it uses an independent writer-handle clone
(`Supervisor::writer_handle`) so a cancel can be sent to a backend even
while a `session/prompt` call against that same backend is already
in-flight and holding the process lock for its own read loop.

## Concurrency model

Two locks matter, and the entire concurrency design is about never
holding either one across real backend I/O:

1. **The `Router` lock** (`Arc<Mutex<Router>>`, shared by all
   transports). Held only briefly, for lookups/bookkeeping (session
   registry reads, `Supervisor::ensure_running`'s liveness check,
   spawning the idle scavenger -- see below) -- released *before* any
   actual stdio round trip against a backend. This is what the
   `_shared`-suffixed dispatch functions
   (`dispatch_proxied_shared`/`dispatch_session_new_shared`/
   `dispatch_session_list_real_shared`) exist to guarantee: lock, gather
   what's needed, drop the lock, `.await` the real backend call, only
   re-lock afterward to write back results (session registration,
   persistence, transcript append).
2. **Each backend's own process lock**
   (`SharedBackendProcess = Arc<Mutex<BackendProcess>>`, one per running
   child process, owned by `Supervisor`). A backend's stdin/stdout is a
   single duplex stream with no request/response demultiplexing, so two
   calls against the *same* backend genuinely must serialize -- but two
   calls against two *different* backends proceed fully in parallel,
   since they lock different `Arc<Mutex<..>>` instances.

Net effect: N clients talking to M distinct backend agents scale as M
independent serial queues, not one global serial queue, and the
`Router`'s own lock is never the bottleneck regardless of how long any
one backend call takes.

### Idle-period notifications (the scavenger)

Because a backend's stdio is single-duplex with no demultiplexing,
`session/update` notifications a backend emits *between* client calls
(not framed inside any in-flight request's response) would otherwise sit
unread in the OS pipe buffer until the next call happened to read them.
`Router::spawn_idle_scavenger_if_new` spawns one lightweight task per
physical backend process (keyed by pointer identity, so a crash+respawn
gets its own fresh scavenger for free) that wakes every 75ms and
`try_lock()`s that backend's process mutex -- succeeding only during a
genuinely idle window (a real in-flight call holds the lock for its
whole read loop, so this never races a live call) -- and drains anything
already sitting in the pipe via a zero-duration read. Bare
`session/update`s route to `NotificationHub`; anything else is logged
and discarded, not silently dropped. This only benefits the persistent,
full-duplex transports (stdio, WS) that have a live subscriber to push
to; `POST /rpc` has no such push channel and is unaffected (see
"Transports" below).

## Sessions: identity and lifetime

`SessionRegistry` ([`session_registry.rs`](../acpx-core/src/session_registry.rs))
maps an opaque `GatewaySessionId` (minted by acpx at `session/new`) to a
`SessionEntry { agent_id, backend_session_id, profile_name, cwd,
created_at, last_activity_at, in_flight, in_flight_since, pinned,
custom_idle_ttl }`. This mapping, and the `Supervisor`'s
`HashMap<agent_id, SharedBackendProcess>` of running child processes, are
both **in-memory only, owned by the daemon process, not by any client
transport connection**:

- A WS/stdio disconnect only unsubscribes that connection's
  `NotificationHub` watches (`transport/ws.rs`'s `handle_socket` cleanup
  loop). It never touches `Supervisor` or `SessionRegistry`. A backend
  process is never killed just because a transport connection dropped.
- This means reconnecting a transport (new WS connection, a fresh
  `POST /rpc`) and reusing the same `GatewaySessionId` resumes
  instantly: `Supervisor::ensure_running` returns the already-running
  `Arc` with no respawn, and the session mapping is already present.

### Retention, idle expiry, and the lifecycle reaper

`acpx-session-lifecycle` plan
([`lifecycle.rs`](../acpx-core/src/lifecycle.rs)): a session left idle
(no in-flight turn) too long is safely closed and evicted, but an
in-flight turn is never interrupted by TTL alone. Controlled by
`LifecycleConfig`, all of it overridable via env (see
[`setup.md`](./setup.md)'s environment variable table):

| Policy | Field | Effect |
| --- | --- | --- |
| Idle session TTL | `idle_session_ttl` (default 30m) | An unpinned, not-in-flight session with no activity for this long becomes a reap candidate. |
| Absolute session TTL | `absolute_session_ttl` (default off) | Optional hard ceiling on session age, regardless of activity -- still never interrupts an in-flight turn. |
| Pinning | `SessionEntry::pinned` | Exempts a session from idle/absolute reaping entirely, subject to `max_pinned_sessions_per_tenant`. |
| Per-session TTL override | `SessionEntry::custom_idle_ttl` | Replaces the deployment-wide idle TTL for one session. |
| Capacity | `max_sessions_total` / `max_sessions_per_tenant` | Admission limits enforced before a backend session is even created. |
| Active-turn deadline | `active_turn_deadline` (default off) | Bounds how long a turn may stay in-flight (`SessionEntry::in_flight_since`) before `Router::cancel_stuck_turns` sends the backend a best-effort `session/cancel` and clears in-flight bookkeeping, so a turn that never completes stops being unconditionally reap-exempt. Does not itself close the session -- a later idle-TTL pass is the real backstop. |
| Connector idle shutdown | `connector_idle_shutdown_ttl` (default off) | Once a shared backend process (a supervisor key) has zero referencing live sessions for this long, `Router::reap_unreferenced_backends` stops it. Independent of session TTLs: a session can close while its process is still referenced by a sibling session under the same key. |

A daemon-owned reaper task (`ACPX_LIFECYCLE_REAPER_ENABLED`, ticking
every `ACPX_LIFECYCLE_REAPER_INTERVAL_SECONDS`, wired in
[`main.rs`](../acpx-server/src/main.rs)) drives all three passes each
tick: `Router::reap_expired_sessions` (idle/absolute TTL), `Router::
reap_unreferenced_backends` (connector idle shutdown), and `Router::
cancel_stuck_turns` (active-turn deadline).

### Retention administration (gateway-native, tenant-scoped)

Not ACP methods -- acpx-native JSON-RPC methods under the
`session/retention/*` namespace, dispatched the same way as
`agents/*`/`profiles/*`. Every call is scoped to the caller's own tenant
(`X-Acpx-Tenant`/native tenant identity) and emits a sanitized
`tracing::info!` audit event (tenant id + gateway session id only, never
prompt/transcript content):

| Method | Effect |
| --- | --- |
| `session/retention/get` | Returns one session's pin state, custom TTL, idle/age seconds, and in-flight count. |
| `session/retention/list` | Same, for every session the caller's tenant owns. |
| `session/retention/pin` | Pins a session (exempt from idle/absolute reaping), subject to `max_pinned_sessions_per_tenant`. |
| `session/retention/unpin` | Restores default TTL behavior. |
| `session/retention/set_ttl` | Sets (or clears) a per-session idle-TTL override. |

## Persistence and restart recovery

If `ACPX_DB_PATH` is set, `acpx_core::persistence::PersistenceStore`
(sqlite, `rusqlite::Connection` behind `Arc<Mutex<..>>`, every method
doing its actual query inside `tokio::task::spawn_blocking` so it never
blocks the async runtime) durably records:

- **`sessions`** -- one row per session (`gateway_session_id`,
  `agent_id`/supervisor key, `backend_session_id`, `profile_name`,
  timestamps). Written fire-and-forget from `Router::record_session`.
- **`transcripts`** -- append-only request/response log per session,
  for audit/history (`session/list`-adjacent bookkeeping), also
  fire-and-forget.

This is what survives a daemon restart -- but the OS child processes do
not (`Command::kill_on_drop(true)` in
[`process.rs`](../acpx-conductor/src/process.rs) kills every spawned
backend the moment the daemon drops its `Child` handles), and neither
does any in-memory state (`Router::new` starts with an empty
`Supervisor`/`SessionRegistry`/`NotificationHub` on every boot). Recovery
after a restart is therefore narrower than a live reconnect:

- `Router::rehydrate_session` only fires for `session/load`/
  `session/resume`/`session/delete` -- any other proxied method
  (`session/prompt` etc.) against a session the fresh in-memory registry
  hasn't seen still errors `UnknownSession` until the client explicitly
  calls `session/load` first.
- Without `ACPX_DB_PATH` configured at all, there is nothing to
  rehydrate from -- `SessionNotPersisted`, full loss.
- With it configured, `session/load` respawns the correct backend type
  (re-running `resolve_profile`, idempotent) and forwards the *same*
  backend-native session id to the fresh process. Whether the backend
  agent itself can actually recall prior conversation turns for that id
  is delegated entirely to that backend's own persistence -- acpx's
  `transcripts` table is descriptive/audit data, never replayed into a
  freshly-spawned backend. Verified end-to-end (real second
  `acpx-server` process, real `claude-agent-acp`) by
  `ambient_claude_session_load_survives_a_real_gateway_restart` in
  [`real_ambient_multi_agent_test.rs`](../acpx-server/tests/real_ambient_multi_agent_test.rs).
- When the strict `/acp` bridge is enabled, its virtual session id,
  selected public model, and accepted adapter configuration are persisted
  alongside the native gateway session. After native startup recovery has
  restored that gateway session, the HTTP bridge rebuilds the tenant-scoped
  virtual mapping before serving `/acp` requests. Bridge model changes,
  adapter option changes, and forks update the same durable binding. If
  the initial binding cannot be persisted, ACPX closes the newly-created
  native session rather than leaving an untracked orphan.
- `GET /health` is an authenticated (when `ACPX_AUTH_TOKEN` is set),
  secret-free readiness endpoint. It reports only aggregate durable
  recovery counts and returns `recovering` while any persisted session is
  in the `restoring` state. Individual session ids and recovery errors are
  deliberately excluded. Recovery errors are flattened and capped before
  persistence; a client attempting explicit `session/load` or
  `session/resume` for a still-restoring row receives a retryable error
  rather than starting duplicate backend recovery.

### Durable secret and configuration store

`ACPX_DB_PATH` also enables `Router::enable_durable_config`
(`acpx-server`'s `main.rs`, right after `with_persistence` and before
either transport starts): profiles, centrally-registered MCP servers,
provider config, and every key a profile references now survive a
restart, closing what used to be an in-memory-only gap. Behavior:

- **Encryption at rest.** Secrets are AES-256-GCM encrypted
  (`acpx_core::keystore::MasterKeyring`) before ever reaching sqlite --
  the `secrets` table only ever holds `(key_ref, ciphertext, nonce,
  key_version)`, never plaintext. The keyring itself is a local file
  (default `<ACPX_DB_PATH>.keyring`, override with
  `ACPX_MASTER_KEYRING_PATH`), created with `0600` permissions on first
  use. This is explicitly a local-file key-management tier, not a real
  OS-keychain/KMS integration -- structured so a KMS-backed
  `MasterKeyring` could replace it later without changing any caller
  (every consumer only ever sees an opaque `KeyRef`).
- **Rotation.** `ACPX_MASTER_KEYRING_ROTATE=1` on a given startup mints a
  new keyring version and re-encrypts every persisted secret under it in
  a one-shot pass (`Router::rotate_master_key`); older versions stay in
  the keyring so mid-rotation ciphertext (if a row hasn't been
  re-encrypted yet) still decrypts. Not a schedule -- unset (the
  default) never rotates.
- **Load ordering matters.** `warm_default_profiles` (auto-seeds a
  profile per installed registry agent) runs *after* `enable_durable_
  config`, not before -- it only fills in a profile name that isn't
  already present, so persisted profiles must load first or a restart
  would silently reseed and shadow an operator's customization of one of
  those default-named profiles (e.g. `codex-acp`) every time.
- **Providers stay provisioning-file-first.** There is deliberately no
  `providers/*` JSON-RPC method (see `Router::register_provider`'s doc
  comment), so `ACPX_CONFIG_FILE` remains the primary way to declare
  providers; `enable_durable_config` mirrors whatever `register_provider`
  registers as a best-effort (fire-and-forget) durability backstop on
  top of that, not a replacement for it. `provisioning.rs`'s `apply` is
  correspondingly idempotent across restarts: a `profiles/create`/
  `mcp_servers/create` that collides with something already loaded from
  a *prior* run retries as an update rather than failing, while a true
  duplicate name *within the same file* still fails startup outright.
- Verified end-to-end (real second `acpx-server` process, same
  `ACPX_DB_PATH`) by
  [`durable_secret_store_binary_test.rs`](../acpx-server/tests/durable_secret_store_binary_test.rs);
  the encryption/rotation/load-ordering details are covered in-process by
  [`durable_secret_store_test.rs`](../acpx-core/tests/durable_secret_store_test.rs).

## Transports

All three transports dispatch through the same shared `Router` and are
wired concurrently in [`main.rs`](../acpx-server/src/main.rs) (a
`tokio::select!` between the stdio loop and the HTTP/WS server):

- **stdio** ([`transport/stdio.rs`](../acpx-server/src/transport/stdio.rs))
  -- newline-delimited JSON-RPC over this process's own stdin/stdout, one
  local client, requests processed sequentially. Subscribes to
  `NotificationHub` the same way WS does, writing live update frames to
  an `Arc<Mutex<Stdout>>` shared with the request/response loop so bytes
  never interleave mid-frame.
- **HTTP** (`POST /rpc`, [`transport/http.rs`](../acpx-server/src/transport/http.rs))
  -- stateless request/response, one call per HTTP request. No live push
  channel -- `session/update`s that occur outside a call are still only
  visible bundled into the *next* call's response under `_acpx.updates`
  (the pre-live-notification-hub behavior), by design, not a bug.
- **WebSocket** (`GET /ws`, [`transport/ws.rs`](../acpx-server/src/transport/ws.rs))
  -- persistent, full-duplex; each inbound frame is one JSON-RPC request,
  each `session/new`/`session/load` response subscribes that connection
  to live updates for the resulting session, forwarded via a per-session
  spawned task for as long as the connection or the session lasts.

Auth (`ACPX_AUTH_TOKEN`, constant-time bearer-token compare) applies to
both `POST /rpc` and the WS upgrade; unset means no auth on that
transport (pair with a TLS-terminating reverse proxy for any
non-loopback bind -- acpx itself never terminates TLS).

### The `/acp` compatibility bridge

Opt-in (`ACPX_ACP_BRIDGE_ENABLED=1` + `ACPX_ACP_BRIDGE_CONFIG_FILE`),
mounted on the same HTTP/WS listener at `/acp/rpc` and `/acp/ws`
([`transport/acp_bridge.rs`](../acpx-server/src/transport/acp_bridge.rs)).
Exists for strict ACP clients that expect a plain agent endpoint with a
small, fixed model list rather than acpx's own profile/provider
machinery -- notably **Zed** and **OpenHands**, both of which support ACP
model discovery (`GET /acp/models`, secret-free: id/name/agent id/
availability only). A bridge session:

- Selects a model from `ACPX_ACP_BRIDGE_CONFIG_FILE`'s `models` list
  (`{"id", "agent_id", "model_id", "name"?}`) via `session/set_config_
  option`, or uses `default_model` if the client never picks one --
  *lazily* bound to a real native gateway session on first
  `session/prompt`, not at `session/new`, so a client that only ever
  lists models never spawns a backend process.
- Is a **virtual session**: the client-visible id never equals the real
  gateway session id underneath it (see `bridge_sessions.rs`). An
  unbound virtual session (picked a model, or not, but never prompted)
  is reaped after `unbound_bridge_session_ttl` (default 5m) without ever
  spawning a backend.
- Forwards backend-initiated interactive requests
  (`session/request_permission`, `fs/*`, `terminal/*`) to the bound `/acp`
  client with session-id translation in both directions -- proven live
  against a real Zed checkout, see
  `memory/acpx/gen/plans/acpx-acp-compatibility/reports/zed-e2e-verification.md`.
- Is capped per tenant by `max_virtual_sessions_per_tenant` (bridge
  config file field, default unlimited).

`acpx-acp-bridge` ([`acpx-bridge/`](../acpx-bridge/)) is a small separate
binary a client spawns as its own ACP stdio subprocess; it does nothing
but forward stdio <-> the shared daemon's `/acp/ws`. See
`scripts/openhands-acpx-bridge.sh` for the OpenHands wiring and
`setup.md`'s "ACP compatibility bridge" section for a full worked
example (config file, daemon invocation, Zed/OpenHands client config).

### Admin surface

Optional, separate listener (`ACPX_ADMIN_BIND`, off by default),
authenticated by a *different* token (`ACPX_ADMIN_TOKEN`) than the
client-facing `ACPX_AUTH_TOKEN` -- deliberately not interchangeable, so a
leaked client token can never reach admin routes. Covers custom-agent
CRUD and other operator-only concerns that are not part of the ACP
surface at all; see
[`acpx-server/src/transport/admin.rs`](../acpx-server/src/transport/admin.rs).

## Agent detection and managed vs. native mode

- **Native/unmanaged mode** (no `_acpx.profile` in `session/new`):
  requests go to `ServerConfig::default_agent_id`, spawned via
  `ACPX_BACKEND_CMD`. No profile/provider/keystore machinery is
  consulted at all.
- **Managed mode** (`_acpx.profile` given): `Router` resolves the named
  `Profile` ([`profile.rs`](../acpx-core/src/profile.rs)) to an
  `agent_id`, an optional `ProviderConfig`
  ([`provider.rs`](../acpx-core/src/provider.rs)) plus a resolved key
  from `Keystore` ([`keystore.rs`](../acpx-core/src/keystore.rs),
  **in-memory only, no persistence, no at-rest encryption yet** -- an
  explicit, tracked open risk, not an oversight), and a `SpawnSpec` via
  `launch.rs`, then merges any centrally-registered MCP servers
  (`mcp_servers.rs`) with whatever the client's own `session/new` sent
  (client wins on name collision).
- **Detection** ([`detect.rs`](../acpx-core/src/detect.rs)) is
  best-effort runtime-availability checking per registry entry's
  preferred distribution: `npx`/`uvx` entries check `node`+`npm`/`uv` are
  on `PATH` (the runtime resolves the actual package on demand); `binary`
  entries check `~/.acpx/adapters/<id>/` for an already-fetched copy.
  Surfaced via `agents/list`/`agents/status`.

## Further reading

- [`../COVERAGE.md`](../COVERAGE.md) -- phase-by-phase implementation
  and test-coverage log, including every ACP-compatibility gap found and
  closed, and the honestly-tracked residual gaps (keystore encryption at
  rest, TLS, install progress/job model, `POST /rpc` idle-notification
  visibility, a full per-backend request/response demultiplexer).
- [`file-structure.md`](./file-structure.md) -- file-by-file map of every
  crate.
- [`setup.md`](./setup.md) -- configuring and running the daemon.
- [`development.md`](./development.md) -- building, testing, contributing.
