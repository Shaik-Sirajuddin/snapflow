# File structure

Cargo workspace, `resolver = "2"`, six members
([`Cargo.toml`](../Cargo.toml)). Crates are listed in dependency order
(each depends only on crates above it).

```
acpx/
  Cargo.toml            workspace manifest, shared dependency versions
  Cargo.lock
  README.md              top-level overview + config quick reference
  COVERAGE.md            phase-by-phase implementation/test evidence log (authoritative)
  PHASE6-NOTES.md        phase 6 design notes (historical)
  docs/                  this directory
  scripts/
    self_test.sh          black-box smoke test (real acpx-server + acpx-selftest)
  .github/workflows/
    ci.yml                 fmt / clippy / test jobs, path-filtered to acpx/**
  acpx-proto/
  acpx-registry/
  acpx-conductor/
  acpx-core/
  acpx-server/
  acpx-client/
```

## `acpx-proto` -- shared wire types

No internal dependencies; both server and client SDK depend on this so
wire shapes can never drift apart.

- [`src/lib.rs`](../acpx-proto/src/lib.rs) -- module root.
- [`src/jsonrpc.rs`](../acpx-proto/src/jsonrpc.rs) -- JSON-RPC 2.0
  envelope types (request/response/error).
- [`src/session.rs`](../acpx-proto/src/session.rs) -- `GatewaySessionId`
  and `session/*` payload shapes.
- [`src/agent.rs`](../acpx-proto/src/agent.rs) -- `AgentStatus` and
  `agent/*`/`agents/*` payload shapes.

## `acpx-registry` -- ACP adapter registry client

- [`src/lib.rs`](../acpx-registry/src/lib.rs) -- module root,
  `Distribution` type (npx/uvx/binary).
- [`src/index.rs`](../acpx-registry/src/index.rs) -- fetches/parses the
  official ACP registry index, with a bundled fallback.
- [`src/install.rs`](../acpx-registry/src/install.rs) -- install-runtime
  helpers for fetching a `binary`-distributed adapter.
- `tests/fallback_registry.rs`, `tests/index_fixtures.rs` -- offline
  index-parsing/fallback tests.
- `tests/install_runtime.rs` -- install-path tests.
- `tests/live_registry.rs` -- real network call against the live
  registry, `#[ignore]`d.

## `acpx-conductor` -- backend process supervision

- [`src/lib.rs`](../acpx-conductor/src/lib.rs) -- module root, re-exports.
- [`src/process.rs`](../acpx-conductor/src/process.rs) -- `BackendProcess`
  (spawned child + its stdio handles), `SpawnSpec`, `kill_on_drop(true)`
  (child dies with the daemon).
- [`src/supervisor.rs`](../acpx-conductor/src/supervisor.rs) --
  `Supervisor`: `HashMap<agent_id, SharedBackendProcess>`,
  `ensure_running`/`stop`/`status`, crash-restart with exponential
  backoff (see `backoff.rs`), independent writer-handle clones for
  `session/cancel`.
- [`src/backoff.rs`](../acpx-conductor/src/backoff.rs) -- exponential
  backoff delay calculation + stability-reset threshold.
- [`src/framing.rs`](../acpx-conductor/src/framing.rs) -- newline-
  delimited JSON-RPC framing (`FramedWriter`/reader) over a child
  process's stdio.
- [`src/terminal.rs`](../acpx-conductor/src/terminal.rs) -- `terminal/*`
  method support (spawned subprocess PTY/output bookkeeping for a
  backend's own `terminal/*` requests).
- `tests/supervisor_test.rs` -- spawn/respawn/backoff/status tests
  against synthetic `sh -c` stand-in backends.

## `acpx-core` -- the gateway's brain

- [`src/lib.rs`](../acpx-core/src/lib.rs) -- module root, re-exports
  (`PersistenceStore`, etc.).
- [`src/router.rs`](../acpx-core/src/router.rs) -- **the largest and
  most important file in the workspace**: `Router` struct, `classify`
  (`MethodClass`), `dispatch`/`dispatch_shared` and the `_shared`
  dispatch-function family (lock-briefly-release pattern), session
  rehydration, the idle scavenger, live-notification wiring
  (`LiveNotifyCtx`), transcript/session persistence calls.
- [`src/session_registry.rs`](../acpx-core/src/session_registry.rs) --
  `SessionRegistry`/`SessionEntry`/`BackendSessionId`, the gateway <->
  backend session id mapping.
- [`src/profile.rs`](../acpx-core/src/profile.rs) -- `Profile`
  CRUD store (agent + provider + key ref + launch overrides + attached
  MCP servers).
- [`src/provider.rs`](../acpx-core/src/provider.rs) -- `ProviderConfig`/
  `ProviderKind` (openai/anthropic/litellm) store.
- [`src/keystore.rs`](../acpx-core/src/keystore.rs) -- in-memory API key
  store, `KeyRef` opaque handles (no persistence, no at-rest encryption
  yet -- tracked open risk).
- [`src/launch.rs`](../acpx-core/src/launch.rs) -- derives a backend
  process's env vars/args (`SpawnSpec`) from a resolved provider + key +
  profile launch overrides.
- [`src/mcp_servers.rs`](../acpx-core/src/mcp_servers.rs) -- centrally-
  registered MCP server store + client/central merge logic.
- [`src/detect.rs`](../acpx-core/src/detect.rs) -- best-effort runtime
  detection (`node`/`npm`, `uv`, or a fetched binary) per registry entry.
- [`src/notify.rs`](../acpx-core/src/notify.rs) -- `NotificationHub`:
  live `session/update` fan-out to whichever transport connection
  currently owns a gateway session, decoupled from request/response
  correlation.
- `src/persistence/` -- sqlite-backed durability:
  - [`mod.rs`](../acpx-core/src/persistence/mod.rs) -- schema SQL, module
    root.
  - [`store.rs`](../acpx-core/src/persistence/store.rs) --
    `PersistenceStore` (`Arc<Mutex<rusqlite::Connection>>` +
    `spawn_blocking` per query).
  - [`sessions.rs`](../acpx-core/src/persistence/sessions.rs) --
    `SessionRecord` (durable twin of `SessionEntry`).
  - [`transcripts.rs`](../acpx-core/src/persistence/transcripts.rs) --
    `TranscriptRecord`/`Direction`, append-only request/response log.
  - [`error.rs`](../acpx-core/src/persistence/error.rs) --
    `PersistenceError`.
- `tests/` (23 files) -- one focused integration-style test file per
  concern: dispatch classification, profile resolution, session
  cancel/list/load-rehydration, live notification hub, idle scavenger,
  persistence, fs/terminal/permission-request proxying, prompt content
  passthrough, gateway-native coverage, authenticate, client
  initialize. All exercise `Router` directly (in-process) against
  synthetic `sh -c` stand-in backends -- no mocks of acpx's own types.

## `acpx-server` -- daemon entrypoint (binary crate)

- [`src/main.rs`](../acpx-server/src/main.rs) -- process entrypoint:
  reads `ServerConfig`, builds one `Router`, optionally attaches
  persistence and applies `ACPX_CONFIG_FILE` provisioning, races the
  stdio loop against the HTTP/WS server in a `tokio::select!`.
- [`src/config.rs`](../acpx-server/src/config.rs) -- `ServerConfig`,
  all `ACPX_*` env var parsing.
- [`src/provisioning.rs`](../acpx-server/src/provisioning.rs) --
  `ACPX_CONFIG_FILE` JSON schema (`ProvisioningFile`/`ProfileEntry`),
  applied via the same `Router::dispatch` path a real client's
  `profiles/create`/`mcp_servers/create` calls would use.
- `src/transport/`
  - [`mod.rs`](../acpx-server/src/transport/mod.rs) -- module root.
  - [`http.rs`](../acpx-server/src/transport/http.rs) -- axum app,
    `POST /rpc`, `AuthConfig`/bearer-token enforcement, `SharedRouter`
    type alias.
  - [`ws.rs`](../acpx-server/src/transport/ws.rs) -- `GET /ws` upgrade +
    `handle_socket` request/response + live-forward loop.
  - [`stdio.rs`](../acpx-server/src/transport/stdio.rs) -- stdin/stdout
    JSON-RPC loop, same live-notification wiring as WS.
  - [`live.rs`](../acpx-server/src/transport/live.rs) -- shared
    subscribe/unsubscribe decision logic (`session_id_to_watch`/
    `session_id_to_forget`) reused by both stdio and WS.
- [`src/bin/selftest.rs`](../acpx-server/src/bin/selftest.rs) --
  `acpx-selftest` CLI: black-box smoke client used by
  `scripts/self_test.sh` and `binary_self_test.rs`.
- `tests/` (11 files) -- process-level tests that spawn the real
  `acpx-server`/`acpx-selftest` **binaries** (`CARGO_BIN_EXE_*`), not
  in-process `Router` calls: auth, HTTP/WS transport, concurrency,
  session-cancel concurrency, provisioning-file, single-agent and
  multi-agent end-to-end lifecycle. `real_ambient_multi_agent_test.rs`
  and `real_claude_multi_agent_test.rs` drive a real `claude-agent-acp`/
  `codex-acp` adapter via this machine's ambient CLI login --
  `#[ignore]`d, opt-in via env vars (see
  [`development.md`](./development.md)).

## `acpx-client` -- Rust client SDK

- [`src/lib.rs`](../acpx-client/src/lib.rs) -- module root.
- [`src/raw.rs`](../acpx-client/src/raw.rs) -- raw JSON-RPC request/
  response wrapper (HTTP transport).
- `src/ext/` -- typed convenience helpers over the raw client:
  [`mod.rs`](../acpx-client/src/ext/mod.rs),
  [`sessions.rs`](../acpx-client/src/ext/sessions.rs) (`session/new`/
  `session/prompt`/etc.),
  [`prompt.rs`](../acpx-client/src/ext/prompt.rs) (prompt content
  building),
  [`profiles.rs`](../acpx-client/src/ext/profiles.rs) (profile CRUD),
  [`registry.rs`](../acpx-client/src/ext/registry.rs) (`agents/*`).
- [`examples/basic_session.rs`](../acpx-client/examples/basic_session.rs)
  -- minimal end-to-end usage example.
- `tests/gateway_client_test.rs` -- SDK-level integration test.
