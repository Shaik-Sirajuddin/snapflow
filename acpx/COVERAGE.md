# acpx test coverage matrix

This document tracks, phase by phase, what's implemented in the `acpx`
workspace and what actually has test coverage -- as opposed to what the
plan merely describes. Step numbers below match
`memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md`; update this
file as each subsequent phase lands rather than letting it drift out of
sync with the code. Every row reflects a real `cargo test --workspace`
run and an actual read of the referenced test file(s) -- not an
aspirational claim of what should exist.

As of this update: `cargo test --workspace` passes **134 tests, 0
failures, 2 explicitly `#[ignore]`d** (the live-registry network test,
see Phase 4 below, and the real-adapter multi-agent e2e test, see the
"real ACP adapter end-to-end" section below -- both hit real networks/
processes by design, not run by default). `cargo build --workspace` and
`cargo fmt --all --check` are both clean.

## Phase 0 -- workspace skeleton

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 1 | 6-crate Cargo workspace, `agent-client-protocol` pinned as a single workspace dependency | `Cargo.toml`, `acpx-proto/`, `acpx-core/`, `acpx-conductor/`, `acpx-registry/`, `acpx-server/`, `acpx-client/` | Implicit -- every other crate's tests only run because this compiles | Done |

## Phase 1 -- single-agent ACP passthrough

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 2 | ACP JSON-RPC types (`initialize`, `session/new`, `session/prompt`, `session/resume`, `session/load`, `session/close`) | `acpx-proto/src/session.rs`, `jsonrpc.rs` | `acpx-proto` unit tests (3): `jsonrpc::tests::request_round_trips`, `session::tests::acpx_ext_is_additive_and_stripped_cleanly`, `session::tests::raw_client_without_acpx_ext_is_unaffected` | Done |
| 3 | Spawn one hardcoded backend, frame/deframe newline-delimited JSON-RPC over stdio | `acpx-conductor/src/process.rs`, `framing.rs` | Exercised indirectly by every stand-in-backend test across `acpx-core`/`acpx-server` (real `sh` subprocess framing, not mocked) | Done |
| 4 | `acpx-server` single-client-to-single-backend stdio proxy | `acpx-server/src/transport/stdio.rs` (now rewritten to go through the Phase 2 `Router` rather than a raw 1:1 proxy -- see Phase 2 below) | `acpx-server/tests/e2e_single_agent_test.rs` (1): `framed_roundtrip_through_a_stand_in_backend` | Done (superseded by Phase 2's router-backed stdio transport, still covered) |

## Phase 2 -- multi-agent + gateway API

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 5 | N supervised processes keyed by agent id, restart-on-crash with exponential backoff | `acpx-conductor/src/supervisor.rs`, `backoff.rs` | `acpx-conductor/tests/supervisor_test.rs` (6) + lib unit tests (3) covering restart/backoff/status transitions | Done |
| 6 | Agent auto-detection (npx/uvx runtime-on-PATH check, binary already-fetched check) | `acpx-core/src/detect.rs` | Exercised via `agents/list`/`agents/status` in `acpx-core/tests/agents_gateway_native_test.rs` | Done |
| 7 | `agents/list`/`agents/status` gateway-native API; `agents/install` | `router.rs`'s `dispatch_native`, `acpx-registry/src/install.rs` | `agents_gateway_native_test.rs`; `acpx-registry` unit tests (8) covering npx/uvx runtime checks and binary archive sniff/extract | Done |
| 8 | Session registry (gateway session id -> agent + backend session id), `session/list` aggregation | `acpx-core/src/session_registry.rs` | `session_registry`'s own unit tests (2) + `router_dispatch_test.rs`'s `session_list_aggregates_registered_sessions` | Done |
| 9 | Router for `session/new`/`session/prompt`/`session/resume`/`session/load`/`session/close`/`session/set_mode`/`session/cancel` -- transparent passthrough | `acpx-core/src/router.rs` (`dispatch_session_new`, `dispatch_proxied`) | `router_dispatch_test.rs` (4), `router_test.rs` (3, classification) | Done |
| 10 | Sqlite-backed persistence for session metadata + transcripts, written off the hot path | `acpx-core/src/persistence/{store,sessions,transcripts,error}.rs` | `persistence_test.rs` (6, CRUD); `router_persistence_test.rs` (1, full `session/new` -> sqlite round trip -- previously flaky due to a `FOREIGN KEY` write-ordering race between three independent `tokio::spawn` tasks, fixed by serializing session+transcript writes into one task, see `router.rs`'s `spawn_session_persistence` doc comment; verified with 40 back-to-back isolated runs, 0 failures) | Done |
| 11 | HTTP/WebSocket transport alongside stdio | `acpx-server/src/transport/{http,ws,mod}.rs`, `main.rs` (stdio + HTTP/WS run concurrently against one shared `Router`) | `acpx-server/tests/http_ws_transport_test.rs` (3): POST /rpc round-trip, WS round-trip, `X-Acpx-Profile` header routing (now resolves through the real `ProfileStore`, see Phase 3) | Done |

## Phase 3 -- provider/key management + profiles

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 12 | Provider config model (`openai`/`anthropic`/`litellm`) | `acpx-core/src/provider.rs` (`ProviderKind`, `ProviderConfig`, `ProviderStore` CRUD) | `provider.rs`'s own unit tests (5); `profile_test.rs`'s cross-store tests | Done |
| 13 | API key store | `acpx-core/src/keystore.rs` (`Keystore`, `KeyRef`) -- **in-memory only, no at-rest encryption**, see gaps below | `keystore.rs`'s own unit tests (4, including a `Debug`-never-leaks-secret-material check); `profile_test.rs`'s cross-store tests | Done for the in-memory CRUD surface; encryption-at-rest explicitly deferred (see gaps) |
| 14 | Profile store CRUD (`profiles/create`/`list`/`update`/`delete`) | `acpx-core/src/profile.rs` (store), `router.rs`'s `dispatch_native` (JSON-RPC wiring) | `profile.rs`'s own unit tests (5); `profile_test.rs` (5, cross-store); `profile_resolution_test.rs`'s `profiles_crud_round_trips_via_dispatch` (full JSON-RPC round trip incl. duplicate-create and delete-missing error paths) | Done |
| 15 | Wire codex agent launches to `openai`/`litellm` provider profiles | `acpx-core/src/launch.rs` (`provider_env`: `CODEX_API_KEY`, `CODEX_CONFIG` JSON carrying `openai_base_url`) | `launch.rs`'s own unit tests (7, incl. litellm using the same codex-acp surface as openai); `profile_resolution_test.rs`'s `session_new_with_profile_injects_resolved_provider_env` (env actually reaches a spawned stand-in process) | Done |
| 16 | Wire claude agent launches to the `anthropic` provider profile | `acpx-core/src/launch.rs` (`provider_env`: `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`) | Same `launch.rs` unit tests as step 15 (Anthropic-specific cases) | Done at the env-mapping level. **Not independently verified against a real `claude-agent-acp` process** -- only the researched env var names (see `05-open-risks.md`) were used, no live adapter to test against in this environment |
| 17 | `session/new` resolves `_acpx.profile` -> agent + provider + spawn, falling back to native/unmanaged when omitted | `router.rs`'s `resolve_profile` + `dispatch_session_new` | `profile_resolution_test.rs`: `session_new_with_unknown_profile_errors`, `session_new_with_profile_injects_resolved_provider_env`, `session_new_native_mode_never_touches_profile_store`; `http_ws_transport_test.rs`'s `http_post_rpc_session_new_routes_via_profile_header` (header precedence over inline `_acpx.profile`) | Done |
| 17a | Central MCP server registry, CRUD + merge-by-name into `session/new`'s `mcpServers` (client wins on collision) | `acpx-core/src/mcp_servers.rs` (`McpServerStore`, `merge_mcp_servers`), `router.rs`'s `dispatch_native` + `dispatch_session_new` | `mcp_servers.rs`'s own unit tests (9); `profile_resolution_test.rs`: `mcp_servers_crud_round_trips_via_dispatch`, `session_new_with_profile_merges_central_mcp_servers_with_client_ones_winning`, `session_new_profile_with_no_mcp_servers_leaves_params_untouched` (no-op guarantee for clients that opt out) | Done |

**Followup (now closed, see "Post-self-review" section below):** a
config-file/env surface for actually provisioning `ProviderConfig`s,
central MCP servers, and profiles into a running `acpx-server` --
`ACPX_CONFIG_FILE`, applied in `main.rs` before either transport starts,
built on the same `Router::register_provider`/`Router::dispatch`
(`profiles/create`/`mcp_servers/create`) seams the tests above already
used programmatically. See `acpx-server/src/provisioning.rs`.

## Phase 4 -- deferred adapter installation

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 18 | Registry client (live fetch + bundled `registry.fallback.json` fallback) | `acpx-registry/src/index.rs` | `fallback_registry.rs` (1), `index_fixtures.rs` (1); `live_registry.rs`'s live-network test is `#[ignore]`d by design (no network dependency in the default run) | Done -- pulled forward into Phase 2's work (commit `f502245`), not built as a separate later pass |
| 19 | `agents/install`: npx/uvx runtime-on-PATH confirmation, `binary` archive download+extract (format sniffed from URL, `cmd` treated as opaque) | `acpx-registry/src/install.rs`, wired into `router.rs`'s `dispatch_native` `agents/install` handler | `install_runtime.rs` (3, real `node`/`npm`/`uv` checks against this environment's actual PATH); `install.rs`'s own unit tests (8, zip/tar.gz/tgz sniffing, opaque `cmd` joining, unsupported-platform/missing-runtime error paths); `acpx-client/tests/gateway_client_test.rs`'s `ext_registry_agents_list_and_status_and_install_round_trip` (full client -> gateway -> real `node`/`npm` `RuntimeConfirmed` round trip, this environment has a real node/npm on `PATH`) | Done for `npx`/`uvx`/`binary`-format-sniffing; **not verified on Windows/macOS** (see gaps) |

## Phase 5 -- acpx-client SDK

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 20 | Raw ACP client transport (JSON-RPC-over-HTTP against the gateway's `POST /rpc`) | `acpx-client/src/raw.rs` (`GatewayClient::call`, `ClientError`) -- see the file's doc comment for the documented deviation from the plan's literal wording (a hand-rolled HTTP transport rather than adopting the official `agent-client-protocol` crate's subprocess-stdio-oriented `Client` trait, justified since acpx is a remote-gateway architecture, not a library that owns its own child process) | `gateway_client_test.rs`'s `raw_call_round_trips_a_gateway_native_method`, `raw_call_surfaces_json_rpc_errors_as_client_errors` | Done |
| 21 | `ext/` extension namespace layered additively on top of `raw` -- profile selection/listing, aggregated `session/list`, registry queries | `acpx-client/src/ext/{sessions,profiles,registry}.rs` | `gateway_client_test.rs`: `ext_sessions_list_aggregates_across_the_gateway`, `ext_profiles_create_list_delete_round_trip`, `ext_profiles_create_via_client_then_session_new_via_header_uses_it` (profile header precedence, exercising the real production `X-Acpx-Profile` path end to end from the client) | Done |
| 22 | `ext::registry::install(agent_id)` -- client-initiated installer calling the gateway's `agents/install` | `acpx-client/src/ext/registry.rs` (`agents_list`, `agents_status`, `install`) | `gateway_client_test.rs`'s `ext_registry_agents_list_and_status_and_install_round_trip` -- runs for real against this environment's actual `node`/`npm` (`RuntimeConfirmed` outcome, not mocked) | Done for the blocking request/response shape. **The progress/job-model question from `05-open-risks.md` is NOT resolved** -- `install` is a single blocking call that returns success/failure once the runtime check completes; there is no polling/streamed feedback for a slow first `npx` fetch or a `binary` download+extract, so a caller has no way to show incremental progress. This is a known gap, not an oversight (see Gaps below) |

## Phase 6 -- end-to-end test suite (spans all phases)

| Step | Feature | Implementation | Test coverage | Status |
|---|---|---|---|---|
| 23 | Investigate `agent-client-protocol-test` (official SDK's test-utilities crate) for protocol-level conformance testing | N/A -- investigation only, see `acpx/PHASE6-NOTES.md` | N/A | **Not adopted.** Verified it does not exist on crates.io (`publish = false` in its own `Cargo.toml`, a permanent opt-out, not unpublished-yet); its actual contents (`MockTransport` that panics if invoked, ad hoc scenario fixtures for the SDK's own doctests, a single-client/single-agent `arrow_proxy` demo) don't fit `Router`'s N-backend, session-registry-keyed gateway shape better than the existing real-`sh`-subprocess stand-in pattern already used everywhere in this workspace. `acpx-proto`'s own round-trip tests already cover the "do the wire types round-trip" half of this step. `05-open-risks.md`'s open item is now closed (verified, not adopted) |
| 24 | Gateway-native test suite for surface with no upstream ACP-spec equivalent: Node/npm-missing status distinction, `agents/install` error paths, profile CRUD error paths, `session/list` edge cases -- all via full `Router::dispatch` rather than a store's own unit tests | Exercises `acpx-core/src/{detect,router,profile}.rs` through the real dispatch entry point | `acpx-core/tests/gateway_native_coverage_test.rs` (8): `runtime_missing` status for `agents/status`/`agents/list` (real `PATH` emptied/restored per-test, whole-file-serialized to avoid racing other tests' `sh` spawns), `agents/install`'s `MissingAgentId`/`UnknownAgentId` error paths, `profiles/update`-on-missing (the one CRUD error path `profile_resolution_test.rs` didn't already cover) + a standalone delete-on-never-existed case, `session/list` on an empty registry and aggregating across two distinct agents (via two profiles, since native mode only ever routes to one fixed default agent) | Done |
| 25 | Reusable, backend-agnostic e2e harness modeled on Zed's `common_e2e_tests!` macro pattern, generic over "which registry agent id" | `acpx-server/tests/e2e_agent_lifecycle_harness.rs`'s `agent_lifecycle_e2e_tests!` macro, instantiated once each for Claude/Codex/Gemini's real `registry.fallback.json` ids, driving the real `acpx_core::router::Router` | Same file (4 tests): `claude::detect_install_then_use_round_trip`, `codex::detect_install_then_use_round_trip`, `gemini::detect_install_then_use_round_trip`, `agents_install_reports_runtime_missing_as_an_error_not_a_crash` | Done |
| 26 | Full lifecycle per agent: detection -> installation (incl. Node/npm-missing as a distinct expected failure, not a crash) -> use (`session/new` -> `session/prompt` -> `session/close`) | Same harness | Detection and installation phases run for real against this environment's actual `node`/`npm` and the bundled registry; the "use" phase falls back to the same synthetic `sh -c '...'` stand-in-backend pattern as the rest of the workspace, since no real Anthropic/OpenAI/Google API keys or adapter processes are available in this environment -- documented explicitly in the file's top doc comment as a known limitation, not silently mocked. The Node/npm-missing case is covered as its own standalone test (`PATH` cleared, asserts `Err(RouterError::Install(InstallError::RuntimeMissing { .. }))`, serialized against the other tests in the file via a shared mutex) | Done for the synthetic-backend "use" phase; **no real adapter has ever been exercised end-to-end in this workspace** (tracked in Gaps below, same item as before) |

Combined workspace test count after Phase 6: **113 passed, 0 failed, 1 ignored** (the pre-existing `live_registry` network test), `cargo fmt --all --check` clean, `cargo build --workspace` clean. This is the final phase in `04-phased-plan.md` -- all six phases are now implemented, with gaps honestly tracked below rather than silently left out.

## Post-Phase-6 -- black-box self-test layer (not a new plan phase; closes a real gap the user asked about directly: "do we have end-to-end tests against the actual published/built artifact, not just in-process code")

Every test suite through Phase 6, including the Phase 6 e2e harness, either
compiles `acpx-server`'s source files directly into the test binary via
`#[path]` (it has no `[lib]` target) or drives `acpx_core::router::Router`
in-process. None of them ever booted the actual, already-compiled
`acpx-server` binary as a real OS process and talked to it purely from the
outside -- so a regression in `main.rs` itself (config parsing, the
concurrent stdio/HTTP `tokio::select!`, the real TCP listener) had no test
that could catch it. Three additions close this gap, built as three
parallel disjoint-ownership pieces:

| What | Implementation | Test coverage | Status |
|---|---|---|---|
| Black-box binary test: spawns the real compiled `acpx-server` binary (via cargo's `CARGO_BIN_EXE_acpx-server`) and drives it purely from outside the process over real stdio, real HTTP, and a real WebSocket upgrade | `acpx-server/tests/binary_self_test.rs` | 3 tests: `real_binary_serves_http_rpc_end_to_end` (full `session/new`->`session/prompt`->`session/close` over HTTP against the real process, which itself spawns a real stand-in backend subprocess), `real_binary_serves_websocket_end_to_end`, `real_binary_serves_stdio_end_to_end` | Done |
| `acpx-selftest`: a standalone, publishable diagnostic CLI (separate `[[bin]]` in the same package) for operators/CI to black-box-check an **already-deployed** `acpx-server` over the network -- distinct from `cargo test`, which only ever runs against a checked-out source tree | `acpx-server/src/bin/selftest.rs` (`--target`/`ACPX_SELFTEST_TARGET` resolution, mandatory `session/list`+`agents/list` checks, optional `ACPX_SELFTEST_FULL=1` full round trip that tolerates backend-specific errors as a pass and only hard-fails on transport-level errors) | Manually verified against both an unreachable target (correct `FAIL`/exit 1) and a real locally-spawned `acpx-server` (correct `PASS`/exit 0, 38 real registry agents reported); no `cargo test` coverage by design, since it's meant to be run standalone post-deployment, not as part of the in-repo test suite | Done |
| `scripts/self_test.sh`: one-shot wrapper tying it together for a human/CI -- builds the workspace, boots a real `acpx-server` against a stand-in backend on an ephemeral port, runs `acpx-selftest` against it, propagates its exit code | `scripts/self_test.sh` (`README.md`'s new `## Self-test` section documents it) | Run twice manually end-to-end during development (real build, real server, real `acpx-selftest`), both passed; also verified correct FAIL propagation against an unreachable port | Done |

Combined workspace test count after this addition: **116 passed, 0 failed,
1 ignored**, `cargo fmt --all --check` clean, `cargo build --workspace`
clean.

One real bug was found and fixed while building `scripts/self_test.sh`:
`acpx-server`'s stdio transport races its HTTP transport in a
`tokio::select!` (`main.rs`), so backgrounding the process naively left
stdin at immediate EOF and the whole process exited within ~100ms even
though the HTTP listener was healthy. Fixed by holding a FIFO open
read-write on a spare fd and feeding the server's stdin from that, which
keeps stdin open for the process's lifetime without leaking a background
`sleep` process. This is an operational footgun for anyone else trying to
run `acpx-server` as a backgrounded shell job with no live stdin --
worth keeping in mind if a real init system/systemd unit is written later
(not yet tracked as a `05-open-risks.md` item; added here since it's a
concrete deployment gotcha discovered empirically, not a design risk).

## Post-self-test -- real multi-agent concurrency fix

Every test through the self-test layer above still used at most one
in-flight request at a time per test, and the transport layer held one
whole-`Router` `Arc<Mutex<Router>>` lock for an entire `dispatch()` call
-- including backend LLM latency. Two concurrent `session/prompt` calls
to *different* agents fully serialized behind that one lock, defeating
the entire point of "multi-agent" for any real deployment with more than
one client. Fixed:

- `acpx-conductor/src/supervisor.rs`: `Supervisor::ensure_running` now
  returns `SharedBackendProcess = Arc<tokio::sync::Mutex<BackendProcess>>`
  instead of an exclusive `&mut BackendProcess` borrow tied to the
  `Supervisor`'s own lifetime; `status()` uses `try_lock` to stay
  non-blocking.
- `acpx-core/src/router.rs`: added `dispatch_shared`/
  `dispatch_session_new_shared`/`dispatch_proxied_shared` free functions
  that lock the `Router` only for gateway-state bookkeeping (session
  registry, profile resolution, `Supervisor::ensure_running`), then drop
  that lock before doing the actual backend stdio round trip against just
  that backend's own per-process mutex. The original `&mut self`
  `Router::dispatch` (used by the ~100 in-process tests elsewhere in this
  matrix) is untouched, so none of them needed to change.
- `acpx-server/src/transport/{http,ws,stdio}.rs`: now call
  `dispatch_shared` instead of `router.lock().await.dispatch(...)`.
- `classify()` gained `session/set_config_option` -> `Proxied` (a real,
  published ACP extension method used by `claude-agent-acp` for
  in-session model selection -- discovered while building the
  real-adapter e2e test below; previously unroutable as `Unknown`).

| What | Implementation | Test coverage | Status |
|---|---|---|---|
| Two different agents' `session/prompt` calls run in parallel, not serialized behind one gateway-wide lock | `acpx-core/src/router.rs`'s `dispatch_shared` family | `acpx-server/tests/concurrency_test.rs` (1): two synthetic backends each sleeping 1.5s, asserts wall-clock stays near 1x, not 2x -- manually verified this test correctly fails (~3s) when reverted to the old single-lock pattern | Done |

## Post-self-test -- reverse-direction `session/update` aggregation fix

`read_matching_response` previously logged-and-dropped every backend
message that didn't match the pending request's id -- i.e. every
`session/update` notification. Discovered while building the real-adapter
e2e test below: real ACP adapters (verified against
`@agentclientprotocol/claude-agent-acp`) deliver the actual assistant
reply text *only* via `session/update` `agent_message_chunk`
notifications streamed during `session/prompt`; the JSON-RPC result
itself is just `{stopReason, usage}`. A client talking to a real backend
through acpx got a technically-successful response with **no visible
reply text in it at all**, ever -- serious enough that it made "acpx
client working end to end against a real backend" false regardless of
anything else in the gateway working correctly.

Fixed in `acpx-core/src/router.rs`:

- `read_matching_response` now returns `(matched_response,
  Vec<unmatched_notifications>)` instead of dropping unmatched messages.
- `attach_updates(response, notifications)` folds notifications into
  `response["_acpx"]["updates"]` -- a no-op (response left byte-for-byte
  untouched) when nothing streamed, so every pre-existing synthetic-backend
  test in this workspace continued to pass unmodified.
- All four backend-round-trip call sites (`dispatch_session_new`,
  `dispatch_proxied`, and their `_shared` twins) now destructure and call
  `attach_updates`.
- `acpx-client/src/raw.rs`: `GatewayClient::call_with_updates()` returns
  `(result, updates)` alongside the existing `call()`.
- `acpx-client/src/ext/prompt.rs` (new): `prompt::send()` convenience
  wrapper plus `extract_message_text()`, concatenating every
  `agent_message_chunk`'s text in streaming order.

| What | Implementation | Test coverage | Status |
|---|---|---|---|
| Streamed `session/update` notifications are aggregated into the JSON-RPC response rather than silently dropped | `acpx-core/src/router.rs`'s `read_matching_response`/`attach_updates` | `acpx-core/tests/session_update_forwarding_test.rs` (2): aggregation actually works, and is a byte-for-byte no-op when nothing streamed | Done |
| Client-side convenience extraction of the assistant's reply text | `acpx-client/src/ext/prompt.rs` | 3 unit tests in the same file: concatenation order, thought-chunks ignored, empty-updates-yields-empty-string | Done |

## Post-self-test -- real ACP adapter end-to-end (closes this workspace's biggest remaining gap)

Every test through every phase above, including the Phase 6 e2e harness,
used a synthetic `sh -c '...'` stand-in backend -- never a real,
published, `npx`-installed ACP adapter. `acpx-server/tests/
real_claude_multi_agent_test.rs` (new, `#[ignore]`d + gated on
`ACPX_LIVE_TEST_ANTHROPIC_BASE_URL`/`ACPX_LIVE_TEST_ANTHROPIC_API_KEY`,
matching `live_registry.rs`'s existing skip-not-fail convention) closes
this for real: it spawns the real, already-compiled `acpx-server` binary,
which spawns two real `npx @agentclientprotocol/claude-agent-acp` child
processes (one per profile), talking to a real
Anthropic-Messages-API-compatible endpoint serving `claude-haiku-4-5`
(the cheapest/fastest model available, selected via the real
`session/set_config_option` extension), through the real `acpx-client`
SDK (`raw::GatewayClient` + `ext::prompt`/`ext::profiles`) -- proving
"acpx daemon + acpx client end to end" together, not the daemon alone.
Both profiles hold independent two-turn conversations
**concurrently** (`tokio::join!`), re-proving the multi-agent concurrency
fix above against real backend processes and real network latency, not a
synthetic `sleep`. Run:

```
ACPX_LIVE_TEST_ANTHROPIC_BASE_URL=<endpoint> \
ACPX_LIVE_TEST_ANTHROPIC_API_KEY=<key> \
cargo test -p acpx-server --test real_claude_multi_agent_test -- --ignored --nocapture
```

Getting this test to actually pass surfaced **three more real bugs**,
none of which any synthetic-backend test in this workspace could ever
have caught, since synthetic stand-in scripts answer any request
uniformly regardless of protocol ordering or schema strictness:

1. **`main.rs` exited the entire process, HTTP/WS included, if stdin hit
   EOF.** `tokio::select!` between the stdio task and the HTTP task meant
   any launch with closed/`/dev/null` stdin -- exactly what this e2e
   test's `Stdio::null()` child does, and exactly what a real
   daemonized/systemd/nohup deployment does -- tore the whole daemon down
   within milliseconds of starting, before it could accept a single HTTP
   connection. Every pre-existing binary test avoided tripping this only
   by keeping stdin piped-and-open for the process's lifetime, masking
   the bug rather than covering it. Fixed: stdio hitting clean EOF now
   falls through to just awaiting the HTTP task instead of ending the
   process; only a genuine stdio *error* still ends it early. Regression
   test: `acpx-server/tests/binary_self_test.rs`'s
   `real_binary_with_closed_stdin_still_serves_http`.
2. **acpx never performed the ACP `initialize` handshake with backend
   processes at all.** Every dispatch path wrote `session/new` as the
   very first message on a freshly spawned backend's stdio. A real
   adapter (verified against `claude-agent-acp`) won't return a proper
   `session/new` result before it has seen `initialize` first --
   surfaced through acpx as an opaque `MissingBackendSessionId`, not any
   kind of protocol error. Fixed in `acpx-core/src/router.rs`:
   `ensure_backend_initialized` performs the real `initialize`
   request/response round trip against a backend process exactly once,
   gated on a new `BackendProcess::handshake_done` flag (owned in
   `acpx-conductor/src/process.rs`, deliberately just a generic
   done/not-done flag with no ACP semantics baked into that
   protocol-agnostic crate) that resets to `false` on every fresh spawn,
   so a crash+respawn mid-session is naturally re-initialized too. Wired
   into all four backend-round-trip call sites. Kept in `Router` rather
   than `Supervisor` deliberately -- an earlier attempt at putting the
   handshake in `Supervisor::ensure_running` itself broke that crate's
   own protocol-agnostic crash/backoff unit tests, which intentionally
   spawn processes that never speak any protocol at all
   (`acpx-conductor/tests/supervisor_test.rs`).
3. **A real backend's JSON-RPC `error` response to `session/new` was
   masked as a generic "missing sessionId" error, hiding the actual
   rejection reason.** Diagnosed by manually driving `claude-agent-acp`
   outside of acpx and finding a real `-32602 Invalid params` error
   (`mcpServers` is a required field in the real ACP schema, not
   optional -- this workspace's own e2e test had omitted it, since
   nothing about acpx's design injects fields a raw ACP client didn't
   itself supply, per `session/new`'s "stays a raw-ACP drop-in" design
   goal). Fixed defensively regardless: `router.rs`'s new
   `extract_backend_session_id` helper now returns a proper
   `RouterError::BackendSessionNewError` carrying the backend's actual
   `error` object when one is present, instead of silently falling
   through to the generic missing-field message.

| What | Implementation | Test coverage | Status |
|---|---|---|---|
| Real `claude-agent-acp` adapter, two profiles, concurrent two-turn conversations, through the real `acpx-server` binary and the real `acpx-client` SDK | `acpx-server/tests/real_claude_multi_agent_test.rs` | 1 test (`#[ignore]`d, network/credential-gated): passed against a real Anthropic-Messages-API-compatible endpoint, both conversations' real model replies (`PONG`/`PANG`) verified, both profiles' real `npx` child processes ran and finished concurrently (~11s wall-clock for two full cold `npx` starts + 4 real model turns total -- not ~2x that, confirming genuine overlap, not serialization) | Done |
| Daemon survives closed/absent stdin while still serving HTTP/WS (systemd/nohup/backgrounded-launch shape) | `acpx-server/src/main.rs` | `acpx-server/tests/binary_self_test.rs`'s `real_binary_with_closed_stdin_still_serves_http` | Done |
| Real ACP `initialize` handshake performed before any other request reaches a freshly spawned backend | `acpx-core/src/router.rs`'s `ensure_backend_initialized`, `acpx-conductor/src/process.rs`'s `BackendProcess::handshake_done` | Exercised implicitly by every pre-existing synthetic-backend test (numeric request id `0`, chosen so those tests' id-echoing shell scripts keep working unmodified) plus the real-adapter e2e test above, which would fail immediately without it | Done |

Combined workspace test count after this addition: **123 passed, 0
failed, 2 ignored** (`live_registry`'s network test, unchanged; the new
real-adapter test, gated on live credentials), `cargo fmt --all --check`
clean, `cargo build --workspace` clean.

## Gaps / not yet covered

Pulled from `memory/acpx/gen/plans/acp-gateway-daemon/05-open-risks.md` --
these are acknowledged, not newly discovered:

- **`ext::registry::install`'s progress/job model is still undecided** (Phase 5 step 22) -- the client can now trigger installation for real, but a slow `npx`/`binary` install has no way to report incremental progress back to a waiting caller; `05-open-risks.md`'s "client-initiated installer needs a progress/job model" item is directly relevant now that this call path exists, not just a future concern.
- **No live-registry test runs by default.** `acpx-registry/tests/live_registry.rs`'s `live_registry_matches_expected_shape` is `#[ignore]`d (hits the real network); only `registry.fallback.json` parsing is covered in the default test run.
- **Resolved for `claude-agent-acp` and `codex-acp` on this machine (via ambient CLI auth), still open for Gemini.** `acpx-server/tests/real_claude_multi_agent_test.rs` drives real `claude-agent-acp` with externally-supplied credentials; `acpx-server/tests/real_ambient_multi_agent_test.rs` (see "Post-provisioning" section above) drives both real `claude-agent-acp` *and* real `codex-acp` using this machine's own already-logged-in CLI sessions, no credentials supplied by acpx at all -- both passed for real. Gemini was never attempted live (no ambient `gemini` CLI login on this machine). Phase 6 step 26's harness (`acpx-server/tests/e2e_agent_lifecycle_harness.rs`) still swaps in the synthetic stand-in for its "use" phase for all three agents, unchanged -- that harness's job is detect/install coverage across all three registry ids uniformly, not real-conversation coverage, which now lives in the two `real_*` test files instead.
- **No Windows/macOS test coverage for the `binary` distribution's download+extract path** -- `install.rs`'s zip/tar.gz sniffing is unit-tested, but only exercised on Linux in this environment; `05-open-risks.md` explicitly calls out that this path needs testing on all three OSes before being considered done.
- **No encryption at rest for the keystore.** `keystore.rs` is explicit in its own doc comment: secrets live in-memory only, process restart forgets them, and no encryption-at-rest mechanism has been chosen yet (`05-open-risks.md`'s "Key storage mechanism is unspecified" item is still open).
- **`claude-agent-acp`'s `ANTHROPIC_BASE_URL` support is researched, not verified against a real running adapter** -- see Phase 3 step 16's row above and `05-open-risks.md`.
- **One process per profile, not one process per session.** Re-resolving an already-running profile (e.g. after a `profiles/update` changes its provider/key) does not restart its already-running supervised process -- documented as a known gap in `router.rs`'s `resolve_profile` doc comment, tracks `05-open-risks.md`'s "one process per backend vs. one process per session" item.
- **Transport security for remote access: partially resolved.** Optional bearer-token auth now exists (`ACPX_AUTH_TOKEN`, see the "Post-Phase-6 self-review" section below) -- unset by default (binds to `127.0.0.1:8790` with no auth, matching prior behavior). TLS is still entirely unprovided by this transport; `05-open-risks.md`'s item is narrower than before, not closed.
- **Partially resolved: `session/update` notifications are now delivered, agent-initiated *requests* are not.** `session/update` notifications arriving during a call are now aggregated into that call's response (`_acpx.updates`, see the "reverse-direction `session/update` aggregation fix" section above) rather than silently dropped. Still genuinely unresolved: a backend-initiated *request* expecting a reply (e.g. `session/request_permission`) has no way to get one in this request/response-shaped aggregation model -- there is still no live, out-of-band channel for the client to answer a backend's mid-call question. `05-open-risks.md` flags this as unresolved; narrower than before, not closed.
- **Closed: provider/profile provisioning surface.** See the "Post-self-review -- `ACPX_CONFIG_FILE` startup provisioning" section below.
- **`mcp_servers/list` does not redact its entries, unlike `profiles/list`'s `launch_overrides` (see the self-review section below).** Centrally-registered MCP server entries are opaque, schema-free `serde_json::Value` blobs (`mcp_servers.rs`'s own doc comment -- "acpx never interprets an MCP server entry's fields itself"), and the real MCP config shape can carry credentials in arbitrary fields (`env`, `headers`, etc. depending on the server's transport). Unlike `launch_overrides`, where the field name and semantics are fixed and known, there is no reliable way to heuristically redact an unconstrained JSON blob without either missing real secrets in an unexpected field or corrupting legitimate non-secret config. Left undone deliberately rather than shipping a fragile guess -- noted here so it isn't mistaken for "already covered by the launch_overrides fix."

All six phases in `04-phased-plan.md` are now implemented. No phase remains unstarted; the gaps listed above are the honestly-tracked residual work, not missing phases.

## Post-Phase-6 self-review: concurrency, multi-client, auth, memory (real bugs found and fixed)

A targeted self-review of concurrency, multi-client handling, auth, and
memory behavior (prompted directly, not part of the original phased
plan) found and fixed three real, concrete bugs -- not style nits. All
three ship with regression tests; the full `cargo test --workspace` run
and a fresh live run of `real_claude_multi_agent_test.rs` (see below)
both stay green.

1. **`session/close` never evicted the session from `SessionRegistry`
   (unbounded memory leak + stale-session correctness bug).**
   `Router::dispatch_proxied` and its concurrency-path twin
   `dispatch_proxied_shared` (`acpx-core/src/router.rs`) both persisted a
   `session/close` to sqlite but never called `SessionRegistry::remove`
   -- a method that already existed and had test coverage of its own,
   but was never called from anywhere in the dispatch path. Practical
   impact for a long-running daemon: every session ever opened stayed in
   the in-memory `HashMap` forever (unbounded growth over the process's
   lifetime), `session/list` kept reporting closed sessions as live
   indefinitely, and a `session/prompt` against an already-closed gateway
   session id still resolved and forwarded to the backend instead of
   erroring. Fixed in both dispatch paths; regression tests:
   `session_close_evicts_session_from_registry_and_rejects_further_use`
   and `dispatch_shared_session_close_evicts_session_too`
   (`acpx-core/tests/router_dispatch_test.rs`).
2. **`profiles/delete` never stopped the profile's supervised backend
   process (orphaned child process leak).** `Router::dispatch_native`'s
   `"profiles/delete"` arm removed the `ProfileStore` entry but left
   whatever OS process had been spawned for that profile (supervisor key
   `"profile:<name>"`, see `resolve_profile`) running indefinitely, with
   no remaining way to ever stop it. Fixed: `profiles/delete` now also
   calls `Supervisor::stop` on that key (best-effort/no-op if the
   profile was never actually used). Regression test:
   `profiles_delete_stops_the_profiles_running_backend_process`, which
   asserts the process is genuinely `Running` after `session/new` and
   genuinely `NotStarted` after `profiles/delete` via a new
   `Router::process_status` test/observability seam.
3. **No auth on the HTTP/WS transport -- closes (half of) the
   previously-open "Transport security for remote access" gap.** Every
   `POST /rpc` and `GET /ws` request was answerable by any client able to
   reach the bound address, including full profile/provider/key
   management and control over every other client's sessions -- a real
   gap for a gateway explicitly designed to serve multiple concurrent
   clients. Fixed: optional bearer-token auth, gated on `ACPX_AUTH_TOKEN`
   (`acpx-server/src/config.rs`). Unset (the default) leaves every
   pre-existing test and deployment byte-for-byte unauthenticated, as
   before. When set: `POST /rpc` requires `Authorization: Bearer
   <token>` or gets a `401` with a JSON-RPC-shaped `-32001` error body
   (`transport::http::AppState`/`AuthConfig`); `GET /ws`'s upgrade
   request is checked the same way (the only point in a WS connection's
   lifetime headers are available) and a missing/wrong token gets the
   upgrade rejected outright rather than completing then failing later.
   Token comparison is constant-time (manual XOR-accumulate, no new
   dependency). `acpx-client::raw::GatewayClient` gained
   `with_auth_token(..)` so the client SDK can actually talk to an
   authenticated gateway, not just the daemon gaining a feature nothing
   in this workspace could exercise. New tests:
   `acpx-server/tests/auth_test.rs` (7 tests: unauthenticated baseline,
   correct/missing/wrong token on `POST /rpc`, correct/missing/wrong
   token on the `GET /ws` upgrade) and
   `acpx-client/tests/gateway_client_test.rs`'s
   `client_with_auth_token_round_trips_against_an_authenticated_gateway`
   (proves the client SDK's new auth support end to end, not just the
   server side). **Still open:** no TLS -- a bearer token sent over
   plaintext HTTP is only as safe as the transport it rides on; pair
   `ACPX_AUTH_TOKEN` with a TLS-terminating reverse proxy for any
   non-loopback deployment. The "Transport security for remote access"
   gap bullet above is updated to reflect auth being closed, TLS still
   open.

Re-verified against a real backend after these fixes (not just the
synthetic stand-ins the regression tests above use):
`real_claude_multi_agent_test.rs` (two real `claude-agent-acp`
processes, two profiles, concurrent two-turn conversations) still
passes end to end, ~33s wall-clock this run (network-latency variance
from the ~11s seen in the original run, not a regression -- both
conversations' real model replies were still verified correct and both
profiles' processes still ran concurrently, not serialized).

**Fourth bug found in the same pass, after the above three were
already fixed and re-verified:** `profiles/create`/`update`/`list`
echoed a profile's `launch_overrides` map back byte-for-byte, with no
redaction, in every response. `launch_overrides` is documented
(`profile.rs`, `resolve_profile`) as a raw env-var escape hatch
specifically meant to carry things like `ANTHROPIC_API_KEY` directly --
exactly what `real_claude_multi_agent_test.rs` itself uses, since no
`ProviderConfig`/`keystore`-based Anthropic wiring test surface exists
yet. Unlike the `secret` field (deliberately never echoed, only its
opaque `KeyRef`), `launch_overrides` values had no equivalent
protection. For a gateway explicitly designed to serve multiple
concurrent clients sharing one `ACPX_AUTH_TOKEN` (see bug 3 above), that
meant any client able to call `profiles/list` could read every other
client's raw secrets in plaintext. Fixed: new
`router::redact_launch_overrides` masks every `launch_overrides` value
(keys stay visible) in the JSON echoed back by `profiles/create`,
`profiles/update`, and every entry in `profiles/list` -- the stored
`Profile` itself, and therefore real backend env injection at spawn
time, is untouched (response-serialization-only redaction). Regression
test: `launch_overrides_values_are_redacted_in_every_profile_response`
(`acpx-core/tests/router_dispatch_test.rs`), which also asserts the
profile stays fully usable (`session/new` still succeeds) after
redaction. Re-verified live: `real_claude_multi_agent_test.rs` (which
sets `ANTHROPIC_API_KEY` via exactly this `launch_overrides` path) still
passes end to end post-fix -- the real key still reaches the spawned
backend, only the client-visible JSON-RPC response no longer echoes it.

Combined workspace test count after all four fixes: **135 passed, 0
failed, 2 ignored**, `cargo fmt --all --check` and `cargo build
--workspace` both clean.

## Post-self-review -- `ACPX_CONFIG_FILE` startup provisioning (closes the last Phase 3 followup)

The one item Phase 3 explicitly left open (see that phase's "Followup"
note above): `Router::register_provider`/`Router::store_key` existed only
as a programmatic seam exercised by this workspace's own tests --
deploying `acpx-server` with a real provider/profile required writing
Rust, not configuring it. Closed by a new `ACPX_CONFIG_FILE` env var,
applied in `main.rs` after persistence setup and before either transport
(`stdio`/`HTTP`/`WS`) starts accepting requests.

| What | Implementation | Test coverage | Status |
|---|---|---|---|
| JSON provisioning file: providers, central MCP servers, profiles (incl. `secret`/`secret_env` for keeping raw secrets out of the file itself) applied via `Router::register_provider` + the real `profiles/create`/`mcp_servers/create` JSON-RPC dispatch path (one validation code path, not a second one) | `acpx-server/src/provisioning.rs` (`load`, `apply`), wired into `main.rs` | 6 unit tests in `provisioning.rs` (apply order, `secret_env` resolution, both-secret-fields-set error, missing-env-var error, unknown-provider-ref deferred-to-resolve-time behavior documented explicitly, `load` round trip against a real temp file) | Done |
| Fails startup outright (non-zero exit, before either transport opens) on a malformed file or a rejected entry (e.g. duplicate profile name) rather than booting a partially-configured gateway | `main.rs`'s `unwrap_or_else(\|err\| panic!(...))` around both `provisioning::load` and `provisioning::apply` | `acpx-server/tests/provisioning_binary_test.rs`'s `real_binary_refuses_to_start_with_an_invalid_provisioning_file` -- spawns the real compiled binary with a duplicate-profile-name config, asserts non-zero exit within 5s and that the HTTP listener never opened | Done |
| End-to-end against the real compiled binary: a provisioned profile is actually usable via `session/new` | `acpx-server/tests/provisioning_binary_test.rs`'s `real_binary_applies_a_provisioning_file_at_startup` -- boots the real binary with a provisioning file, confirms `profiles/list` reflects it, then completes a real `session/new` against the provisioned profile through a real spawned stand-in backend | Same file | Done |

Unset (the default) leaves every pre-existing deployment/test
byte-for-byte unchanged -- `ACPX_CONFIG_FILE` is opt-in. Does not add
encryption at rest for the keystore itself (still open, see Gaps above);
`secret_env` only keeps the config *file* free of secrets, which is a
narrower, more achievable win for the common env-injected-by-orchestrator
deployment shape.

Combined workspace test count after this addition: **143 passed, 0
failed, 2 ignored**, `cargo fmt --all --check` and `cargo build
--workspace` both clean.

## Post-provisioning -- real `codex-acp` adapter end-to-end via ambient CLI auth (partially closes a Gaps-section item)

The user pointed out this machine already has `claude`/`codex` CLIs
installed and logged in ("we already have claude, codex binaries in this
system, you can use that") -- no fabricated credentials needed to
exercise a real adapter, unlike `real_claude_multi_agent_test.rs` which
requires externally-supplied `ACPX_LIVE_TEST_ANTHROPIC_*` values. This
closes the `codex-acp` half of the "resolved for claude-agent-acp, still
open for codex-acp/gemini" gap below (Gemini remains open -- no ambient
`gemini` CLI login available on this machine).

Manually verified first via `curl` against a live `acpx-server` process
started with an `ACPX_CONFIG_FILE`-provisioned `claude-ambient`/
`codex-ambient` profile pair (no `provider`/`launch_overrides` at all):
`agents/list` correctly reported both `claude-acp` and `codex-acp` as
`installed` (real `node`/`npm` on `PATH`, live-registry-fetched entry
list -- not just the 3-agent `registry.fallback.json`, confirming this
environment has live network access to the real ACP registry too);
`session/new` against each profile spawned the real `npx`-distributed
adapter, which inherited this process's ambient environment and found
its own already-authenticated session (`~/.claude/.credentials.json` for
claude-agent-acp; the local codex CLI's bifrost-backed auth store for
codex-acp) with zero acpx-supplied credentials; a real `haiku` call
replied `PONG` (real cost `$0.046591` billed to the ambient Claude
account -- a real, small, actual charge, not simulated); a real
`codex/gpt-5.4-mini[low]` call (this machine's own bifrost model catalog,
fetched live via `session/new`'s `models.availableModels`) replied
`PANG`, streamed as `agent_message_chunk` updates (`"P"` + `"ANG"`) the
same way `claude-agent-acp` does.

Automated as `acpx-server/tests/real_ambient_multi_agent_test.rs` so this
is reproducible rather than a one-off manual check: spawns the real
`acpx-server` binary, asserts `agents/list` reports both `claude-acp`/
`codex-acp` as `installed`, creates two profiles with **no `provider`/
`launch_overrides`**, then runs one real conversation turn against each
concurrently (`tokio::join!`), asserting the real model replies contain
`PONG`/`PANG` respectively. `#[ignore]`d and gated on
`ACPX_LIVE_TEST_AMBIENT=1` (not credential env vars, since the
credentials are this machine's own ambient CLI login state, not
something a caller supplies) -- makes real billed API calls and
hardcodes a model id (`codex/gpt-5.4-mini`) specific to this machine's
own bifrost-backed codex catalog, so it stays opt-in rather than running
in a shared/default `cargo test` invocation. Actually run and passed:
`ok. 1 passed; 0 failed` in 13.81s.

| What | Implementation | Test coverage | Status |
|---|---|---|---|
| Real `claude-agent-acp` + real `codex-acp`, both via this machine's ambient CLI auth (no acpx-supplied credentials), detected + spawned + prompted concurrently through the real `acpx-server` binary and real `acpx-client` SDK | `acpx-server/tests/real_ambient_multi_agent_test.rs` | 1 test (`#[ignore]`d, opt-in via `ACPX_LIVE_TEST_AMBIENT=1`, no external credential vars needed): run and passed on this machine, both real model replies (`PONG`/`PANG`) verified | Done for `claude-acp`/`codex-acp` on this machine; Gemini still unattempted (no ambient `gemini` CLI login here) |

Combined workspace test count after this addition: **144 passed, 0
failed, 3 ignored** (adds this new opt-in test to the prior 2), `cargo
fmt --all --check` and `cargo build --workspace` both clean.

## ACP compatibility hardening, phase 1 -- real `agentCapabilities` surfaced from the `initialize` handshake

Prompted by a direct user question ("what are the gaps in the ACP
compatibility?") and a follow-up instruction to fix them phase-by-phase,
treating spec compatibility as the priority. A review of the real ACP
spec surface (not just this workspace's own test coverage) turned up
several gaps distinct from the ops/hardening gaps already tracked below.
This phase closes the first and most foundational one: acpx performed
the real `initialize` handshake against every backend (see the
"Post-self-test -- real ACP adapter end-to-end" section above) but threw
away the response entirely once it had unblocked `session/new` --
`ensure_backend_initialized` read up to the matching response `id` and
discarded the value. That means acpx never knew, and never told a
client, what a given backend actually supports: `agentCapabilities`
(`loadSession`, `promptCapabilities`, `mcpCapabilities`), `authMethods`,
or the negotiated `protocolVersion`. Every later compatibility fix
(fs/terminal delegation, `authenticate`, permission requests) needs this
first, since whether to even attempt those depends on what the specific
backend claims to support.

Fixed: `BackendProcess` (`acpx-conductor/src/process.rs`) gained an
`agent_capabilities: Option<serde_json::Value>` field, reset to `None`
alongside `handshake_done` on every fresh spawn (so a crash+respawn
re-captures a fresh value, never serves a stale one). `Router`'s
`ensure_backend_initialized` now stores the real `initialize` response's
`result` object into it instead of discarding it. `session/new`'s two
dispatch paths (`Router::dispatch_session_new` and its
lock-released twin `dispatch_session_new_shared`) now attach it as
`_acpx.agentCapabilities` in the response via a new
`attach_session_new_extras` helper (a `session/new`-specific sibling of
`attach_updates`, so `session/prompt`/etc. don't carry a backend's
one-time `initialize` capabilities on every single call) -- additive and
namespaced, so a raw ACP client that doesn't know about it is unaffected,
matching this gateway's existing `_acpx.updates` convention.

New test: `acpx-core/tests/session_update_forwarding_test.rs`'s
`session_new_surfaces_the_backends_real_initialize_capabilities`, using
a stand-in backend that answers `initialize` with a realistic
`agentCapabilities`/`authMethods`/`protocolVersion` shape (distinguishable
from every other stand-in's generic `{"ok": true}`), asserting
`session/new`'s response carries it verbatim, and that a second
`session/new` against the same still-running process keeps surfacing the
same captured value (proving it survives past the one-shot handshake,
not just a side effect of it). The pre-existing
`session_new_response_has_no_acpx_updates_field_when_backend_emits_none`
test's assertion was updated to match: `_acpx` is no longer guaranteed
absent (its own stand-in's generic `initialize` reply now becomes a
captured, if meaningless, `agentCapabilities` value), only `_acpx.updates`
still is when the backend emits no `session/update` notifications.

Workspace test count after this addition: **144 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace` both
clean.

**Recheck against the full ACP spec surface after this phase** -- gaps
still open, in priority order for subsequent phases:
1. Bidirectional `session/request_permission` (and any other
   agent-initiated request expecting a reply) still has no reply
   channel -- biggest remaining architectural gap.
2. `fs/read_text_file`/`fs/write_text_file` still unimplemented;
   client capabilities still unconditionally declare both `false`.
3. `terminal/*` (`create`/`output`/`wait_for_exit`/`kill`/`release`)
   still entirely unimplemented; no `terminal` capability declared.
4. `authenticate` method still entirely unimplemented on the
   backend-facing side (no code path exists for a backend that requires
   it before `session/new`).
5. Client-facing side (acpx-server as the endpoint external clients
   talk to) still has no `initialize`/`authenticate` handshake of its
   own, and doesn't yet expose the newly-captured `agentCapabilities` as
   a first-class `profiles/*` field (only inline on each `session/new`
   response) -- clients still can't ask "what does this profile support"
   without first opening a session.

These map directly to remaining phases 2-6 of this compatibility
hardening effort. `protocolVersion` sent in the handshake also stays
hardcoded to `1` with no negotiation against what a backend reports back
in its own `initialize` response -- noted here, not yet fixed, low
priority relative to the above since no adapter tested so far has
rejected it.

## ACP compatibility hardening, phase 2 -- `session/request_permission` no longer deadlocks the backend

Closes what phase 1 flagged as the single biggest remaining
architectural gap. Real bug, not hypothetical: `read_matching_response`
(the loop every dispatch path uses to read a backend's stdio until its
own request's `id` shows up) classified *any* message without a matching
`id` as a `session/update`-style notification, including an
agent-initiated *request* like `session/request_permission` -- which
carries its own `id` and a `method`, and, per the real ACP spec, blocks
the backend from producing the outer call's own response until it gets
an answer. Pre-fix, acpx never sent one, so a real backend that ever
asked permission mid-turn (any adapter running a shell/edit tool under
normal safety settings) would hang forever, and so would the client's
own request to acpx.

Fixed in `acpx-core/src/router.rs`: `read_matching_response` now checks
every unmatched message for `id` *and* `method` together (a request, not
a notification) before falling back to treating it as one. If the
method is `session/request_permission`, a new `build_permission_reply`
builds a real, schema-correct reply (`agentclientprotocol.com/protocol/
schema`'s `RequestPermissionResponse`: `result.outcome` is either
`{"outcome": "selected", "optionId": ..}` or `{"outcome": "cancelled"}`)
per a new profile-scoped `crate::profile::PermissionPolicy`
(`AutoAllow`/`AutoReject`, default `AutoReject`) and writes it straight
back to the backend's stdin so it can proceed. Any other agent-initiated
request method (none exist in this workspace yet, but the deadlock risk
is identical for any future one, e.g. `fs/read_text_file`) gets a proper
JSON-RPC `-32601` method-not-found error instead of silence, so it can
never wedge a session even before its real handler exists. ACP's own
spec explicitly sanctions automatic client-side decisions here ("Clients
MAY automatically allow or reject permission requests according to user
settings") -- this isn't a workaround, it's the documented alternative to
a live, synchronous, out-of-band reply channel, which this gateway's
HTTP-shaped transport doesn't have (see the still-open gap below).

Every `{request, reply}` pair handled this way is surfaced, not hidden:
`_acpx.agentRequests` (additive/namespaced, same convention as
`_acpx.updates`/`_acpx.agentCapabilities`) on both `session/new`'s and
every proxied method's response, via `attach_updates`/
`attach_session_new_extras`, both extended with a third parameter.
`SessionEntry` (`acpx-core/src/session_registry.rs`) gained a
`profile_name: Option<String>` field (threaded through
`SessionRegistry::register`'s new third parameter) so a later
`session/prompt`/etc. call on an already-open session can still look its
originating profile's policy back up -- this didn't exist before this
phase; `session/new`'s own dispatch already had the resolved `Profile`
in scope, but nothing downstream did.

New test file: `acpx-core/tests/permission_request_test.rs`, a stand-in
backend that -- on `session/prompt` -- sends a real-shaped
`session/request_permission` request (both an `allow_once` and a
`reject_once` option) and then blocks its own inner `while read` loop on
seeing a reply with that request's id before answering the outer call.
Two tests: default/native mode (no profile) auto-rejects (selects the
`reject-once` option) and the outer call still completes; a profile
created with `"permission_policy": "auto_allow"` auto-allows (selects
`allow-once`) instead. Both wrapped in a 5-second `tokio::time::timeout`
rather than a bare `.await` -- a regression of this fix is a genuine
infinite hang, not a normal assertion failure, so the test needs to fail
fast instead of wedging the whole binary.

Workspace test count after this addition: **146 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace` both
clean.

**Recheck against the full ACP spec surface after this phase:**
1. `fs/read_text_file`/`fs/write_text_file` -- still unimplemented,
   client capabilities still unconditionally declare both `false`. Next
   phase.
2. `terminal/*` -- still entirely unimplemented, no `terminal`
   capability declared.
3. `authenticate` method -- still entirely unimplemented on the
   backend-facing side.
4. Client-facing `initialize`/`authenticate` handshake, and exposing
   `agentCapabilities`/`permission_policy` as first-class `profiles/*`
   fields rather than only inline on `session/new` -- still open.
5. **Newly narrowed, not closed:** `session/request_permission` no
   longer deadlocks, but there is still no live, interactive,
   out-of-band channel for an actual human/client decision to reach a
   backend mid-call -- every decision today is a static, pre-configured
   policy, not a real ask. A true fix would need either a persistent
   per-session push channel on the WS transport (the one transport that
   could support it; `acpx-server/src/transport/ws.rs`'s current
   request/response-per-frame loop doesn't) or an equivalent async
job/callback model on HTTP. Not attempted in this phase -- tracked
here as the honest residual, not silently declared done.

## ACP compatibility hardening, phase 3 -- real `fs/read_text_file`/`fs/write_text_file`

Closes the next item from phase 2's recheck list. `fs/read_text_file`/
`fs/write_text_file` are agent-initiated requests with the exact same
deadlock risk `session/request_permission` had before phase 2 -- a
backend that asks and gets no reply blocks forever. `read_matching_
response`'s agent-initiated-request branch (added in phase 2) now
recognizes both methods specifically instead of falling through to the
generic method-not-found error.

New `Profile::allow_fs_access: bool` (default `false`, opt-in not
opt-out -- see that field's doc comment for why this one gets a stricter
default than `permission_policy`: a backend being able to read/write
arbitrary paths on whatever host runs acpx is a materially different
risk than picking among options the backend itself already offered).
`ensure_backend_initialized` now declares the *real* value in
`initialize`'s `clientCapabilities.fs.{readTextFile,writeTextFile}`
instead of unconditionally `false` -- both flip together per profile,
matching the real ACP capability shape (no separate opt-in per
direction; real adapters don't expect that granularity either based on
the schema). A new `BackendCallPolicy` struct
(`permission_policy` + `allow_fs_access`) replaces passing
`PermissionPolicy` alone into `ensure_backend_initialized`/
`read_matching_response`, computed once per call site via
`BackendCallPolicy::from_profile` -- avoids the parameter list at all
four dispatch call sites growing by one every time a new per-profile
auto-decision knob is added.

A new `handle_fs_request` performs real disk I/O against acpx's own host
filesystem when enabled: `tokio::fs::read_to_string`/`tokio::fs::write`
against the request's `path` verbatim (real ACP clients/editors always
send absolute paths; acpx has no separate notion of a session's
workspace root to resolve a relative one against, same as any other
process). `fs/read_text_file`'s optional `line` (1-indexed start) and
`limit` (max lines) params are honored by windowing the file's lines in
memory. I/O errors (e.g. file not found) become a proper JSON-RPC error
reply (`-32001`, carrying `data.path`) rather than a panic or a silently
swallowed failure. When disabled for the profile (the default), a
request gets a clear "disabled for this profile" error distinct from
"acpx doesn't support this method at all" -- distinguishing a
capability that's off from one that doesn't exist yet.

New tests: `acpx-core/tests/fs_request_test.rs` (real temp files on real
disk, via a stand-in backend using the same "inner `while read` loop
blocks until it sees its request's reply id" trick as
`permission_request_test.rs`, wrapped in the same 5-second timeout
guard): disabled-by-default gets a clear error and the outer call still
completes without touching disk (`write_path` asserted to not exist
afterward); a profile with `"allow_fs_access": true` gets the *real*
file content back (verified against the temp file's actual bytes) and a
real write that's then verified by reading the temp file back directly,
bypassing acpx entirely, to prove the write genuinely landed on disk.
Plus two `acpx-core/src/router.rs`-internal unit tests for
`handle_fs_request`'s `line`/`limit` windowing arithmetic and its
missing-file error path.

Workspace test count after this addition: **150 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace` both
clean.

**Recheck against the full ACP spec surface after this phase:**
1. `terminal/*` (`create`/`output`/`wait_for_exit`/`kill`/`release`) --
   still entirely unimplemented, no `terminal` capability declared. Next
   phase.
2. `authenticate` method -- still entirely unimplemented on the
   backend-facing side.
3. Client-facing `initialize`/`authenticate` handshake, and exposing
   `agentCapabilities`/`permission_policy`/`allow_fs_access` as
   first-class `profiles/*` fields rather than only inline on
   `session/new` -- still open.
4. The live-interactive-decision gap from phase 2's recheck (no
   out-of-band channel for a real human/client answer mid-call) applies
   identically to `fs/*` now too: today's "real I/O, but always
   auto-approved by profile config" is not the same as a client seeing
   and approving each individual file access. Same root cause, same
   honest non-fix as before.

## ACP compatibility hardening, phase 4 -- real `terminal/*`

Closes the next item from phase 3's recheck list. `terminal/create`,
`terminal/output`, `terminal/wait_for_exit`, `terminal/kill`,
`terminal/release` are agent-initiated requests with the same deadlock
risk `fs/*` and `session/request_permission` had before phases 2/3.
`read_matching_response`'s agent-initiated-request branch now recognizes
all five methods, gated on a new `Profile::allow_terminal_access: bool`
(default `false`, same opt-in-not-opt-out rationale as
`allow_fs_access`: spawning arbitrary host processes is at least as
dangerous as arbitrary file I/O). `BackendCallPolicy` gained
`allow_terminal_access` alongside `allow_fs_access`/`permission_policy`.
`ensure_backend_initialized` now declares the real value in
`initialize`'s `clientCapabilities.terminal.{create,output,waitForExit,
kill,release}` instead of omitting the `terminal` capability entirely.

New `acpx-conductor::terminal` module owns the actual process
supervision, kept protocol-agnostic on purpose (same crate-boundary
split as `BackendProcess` itself): `TerminalHandle::spawn` launches a
child with `Stdio::piped()` stdout+stderr, `kill_on_drop(true)`, and two
background tasks that continuously drain both streams into a single
interleaved in-memory buffer (matching real terminal semantics -- ACP
doesn't separate stdout/stderr in `terminal/output`), truncated from the
front to respect `outputByteLimit` if given. `BackendProcess` gained
`terminals: HashMap<String, TerminalHandle>`, keyed by a `term-<uuid>`
id acpx mints in `terminal/create`'s reply; `handle_terminal_request` in
`acpx-core/src/router.rs` implements all five methods against it,
including ACP's `env` param being an array of `{name, value}` objects
(confirmed against the real schema, not JSON's usual object-map shape).
`terminal/release` removes the handle from the map, which drops (and,
via `kill_on_drop`, force-kills if still running) the underlying child --
matching the spec's "id invalid for every other terminal/* method
afterward" without a separate "invalidated" flag to track.

**Real bug found and fixed in this phase, not by a subagent review but
by the phase's own test suite failing on the very first `cargo test`
run:** `wait_for_exit` only awaited `Child::wait()` (the process being
reaped), not the two background capture tasks finishing draining
stdout/stderr. `Child::wait()` resolving and a pipe-reader task being
scheduled to read the child's last buffered bytes are two independent
readiness notifications with no ordering guarantee between them, so a
caller doing `wait_for_exit()` immediately followed by `output()` (the
exact sequence any real ACP client/backend would use, and exactly what
the new integration test does) could observe truncated or even
completely empty output despite the process having already exited and
printed something. Reproduced as a genuine non-deterministic unit-test
failure (`captures_output_and_exit_status`, expected `"hello"` got
`""`) on the first run after implementation, not a hypothetical. Fixed
by keeping each capture task's `JoinHandle` on `TerminalHandle` and
awaiting both in `wait_for_exit` after `Child::wait()` returns, before
recording the exit status -- verified fixed with 20 consecutive clean
runs of the previously-flaky test (`--test-threads=1`, no failures)
after the fix, versus a reproducible failure before it.

New tests: 3 unit tests in `acpx-conductor/src/terminal.rs` (captures
real stdout and real exit code; byte-limit truncation keeps the most
recent bytes, not the oldest; `kill` makes a `sleep 30` child's
`wait_for_exit` return quickly with a non-zero-equivalent status rather
than hanging, under a 5-second timeout guard). New
`acpx-core/tests/terminal_request_test.rs`, mirroring
`fs_request_test.rs`'s stand-in-backend-with-blocking-reply-loop
pattern (5-second timeout guard): disabled-by-default gets a clear
"disabled for this profile" error on `terminal/create` and the outer
call still completes without spawning anything; a profile with
`"allow_terminal_access": true` gets a real minted `terminalId`, the
real exit code (`7`) from a real `sh -c "echo hello; exit 7"` child, the
real captured stdout (`"hello"`), and a successful `terminal/release`,
with all four request/reply pairs surfaced via `_acpx.agentRequests`.

Workspace test count after this addition: **155 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. `authenticate` method -- still entirely unimplemented on the
   backend-facing side. Next phase.
2. Client-facing `initialize`/`authenticate` handshake, and exposing
   `agentCapabilities`/`permission_policy`/`allow_fs_access`/
   `allow_terminal_access` as first-class `profiles/*` fields rather
   than only inline on `session/new` -- still open.
3. `terminal/kill` is implemented but not yet exercised by an
   integration test through the full `read_matching_response` dispatch
   path (only the lower-level `TerminalHandle::kill` unit test and the
   router-level `create`/`wait_for_exit`/`output`/`release` sequence).
   Low risk (same code path as the other four methods, same policy
   gate, same handler function) but noted honestly rather than silently
   assumed covered.
4. The live-interactive-decision gap from phases 2/3's recheck (no
   out-of-band channel for a real human/client answer mid-call) applies
   identically to `terminal/*` now too: every terminal spawn today is
   "real process, but always auto-approved by profile config," not a
   client seeing and approving each individual command before it runs.
   Same root cause, same honest non-fix as before.

## ACP compatibility hardening, phase 5 -- backend-facing `authenticate`

Closes the next item from phase 4's recheck list. Unlike `fs/*`/
`terminal/*`/`session/request_permission` (all agent-*initiated*
requests acpx answers), `authenticate` is client-initiated -- acpx is
the one calling out to the backend. Real ACP schema (agentclientprotocol
.com/protocol/schema, `AuthenticateRequest`/`AuthenticateResponse`):
`initialize`'s response may carry a non-empty `authMethods` array (each
entry an `{id, name, description?}` object); if it does, a client is
expected to send `authenticate` with `params.methodId` set to one of
those ids before `session/new` is expected to succeed.

`ensure_backend_initialized` is restructured: the `initialize` round
trip itself stays gated on `BackendProcess::handshake_done` exactly as
before (never re-sent), but a new second phase runs on *every* call,
driven off the already-cached `initialize` result
(`proc.agent_capabilities`) rather than the wire -- no second
`initialize` is ever sent, since a real adapter has no obligation to
tolerate that. If `authMethods` is empty, this phase is a one-time
no-op (`BackendProcess` gained `authenticated: bool`, flipped `true`
immediately so subsequent calls short-circuit without re-deriving
anything). If non-empty and not yet authenticated: a new
`Profile::auth_method_id: Option<String>` (default `None`, same
opt-in-not-opt-out family as `permission_policy`/`allow_fs_access`/
`allow_terminal_access`) is consulted. `None` -- the default -- means
acpx refuses to even attempt `session/new` against an unauthenticated
backend, returning a new `RouterError::BackendRequiresAuthentication`
carrying every advertised method id, rather than letting the backend's
own downstream rejection (if any -- some adapters might not even
reject cleanly) stand in for a real diagnostic. `Some(method_id)`
drives a real `authenticate` request/response round trip; a JSON-RPC
`error` in the reply becomes `RouterError::BackendAuthenticationError`
(the raw backend error object, not swallowed); success flips
`proc.authenticated = true` and every later call on this process skips
straight past. A failed attempt leaves `authenticated` `false`, so a
later call (e.g. after an operator fixes a typo'd `auth_method_id`)
retries for real rather than being permanently wedged for this
process's lifetime.

`BackendCallPolicy` gained `auth_method_id: Option<String>` alongside
the other three per-profile knobs; picking up an owned `String` here
meant it could no longer derive `Copy` (only `Clone`), so the four
dispatch call sites that use one `BackendCallPolicy` value across both
`ensure_backend_initialized` and `read_matching_response` now
`.clone()` at the first use -- a real, deliberate behavior-preserving
mechanical change, not a workaround for something deeper.

New `acpx-core/tests/authenticate_test.rs`, three cases against a
stand-in backend that advertises `authMethods: [{"id": "api-key", ...}]`
and only answers `authenticate` successfully for that exact id (a
wrong id gets a real JSON-RPC error, matching what a real adapter
rejecting an unrecognized method id would send): (1) a backend that
advertises no `authMethods` at all is unaffected -- `session/new`
proceeds exactly as every pre-existing test already implicitly
exercised; (2) a backend that requires auth, with no
`Profile::auth_method_id` configured (native/unmanaged mode), gets
`RouterError::BackendRequiresAuthentication` naming the one advertised
method, and never reaches the backend's `session/new` handler at all;
(3) a backend that requires auth, with the right `auth_method_id`
configured via `profiles/create`, gets a real `authenticate` round trip
performed for real and then a real, successful `session/new`. All three
wrapped in the same 5-second `tokio::time::timeout` guard as every
other agent-initiated/handshake test in this workspace, since a
regression here is plausibly a hang (e.g. a malformed `authenticate`
request the stand-in backend's `while read` loop never matches), not
just a wrong assertion.

Workspace test count after this addition: **158 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. Client-facing `initialize`/`authenticate` handshake on acpx-server's
   own endpoint (i.e. acpx itself advertising `authMethods` to *its*
   callers, symmetric to what this phase just built for the
   backend-facing side) -- still entirely unimplemented. Next phase,
   alongside exposing `agentCapabilities`/`permission_policy`/
   `allow_fs_access`/`allow_terminal_access`/`auth_method_id` as
   first-class `profiles/*` response fields rather than only inline on
   `session/new`.
2. `authenticate`'s real schema also supports `AuthMethodEnvVar` (the
   client passes credentials to the agent as environment variables) and
   an implicit `AuthMethodAgent` (agent handles it entirely itself, no
   client action needed) as documented method *kinds*, not just a bare
   `{id, name}` pair -- acpx's `auth_method_id` only ever forwards the
   id verbatim and never inspects or acts on a method's kind (e.g.
   auto-injecting an env var for an `AuthMethodEnvVar` entry the way it
   already does for `provider`/`key_ref` at spawn time). Not attempted
   this phase; every real adapter checked so far (claude-agent-acp,
   codex-acp) authenticates ambiently outside ACP entirely and
   advertises no `authMethods`, so this has not yet been exercised
   against a real backend, only the synthetic stand-in above -- noted
   honestly rather than assumed equivalent.
3. The live-interactive-decision gap from phases 2-4's recheck applies
   here too, in a different shape: `auth_method_id` is a static,
   pre-configured choice baked into the profile ahead of time, not a
   client picking a method (or supplying a credential) interactively in
   response to a backend's real advertised options. Same root cause,
   same honest non-fix as before.
