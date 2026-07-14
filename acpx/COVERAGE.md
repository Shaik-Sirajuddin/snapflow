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

## ACP compatibility hardening, phase 6 -- client-facing `initialize`/`authenticate`

Closes the last item from phase 5's recheck list, and turns out to be
the most consequential gap found across this entire hardening series:
**a spec-compliant ACP client/editor always sends `initialize` as its
very first request over the wire, before anything else** (per
agentclientprotocol.com's own documented handshake flow). Every phase
1-5 in this series, and every one of this workspace's ~155 pre-existing
tests, only ever implemented/exercised the *backend*-facing side of
`initialize`/`authenticate` (`ensure_backend_initialized` -- acpx
calling out to whatever process a profile spawns). acpx's own
client-facing endpoint never classified `initialize` or `authenticate`
at all -- both fell through `classify`'s `_ => MethodClass::Unknown`,
so any real ACP editor/IDE connecting to acpx (over any transport:
stdio, HTTP, WS) would have gotten an immediate `UnknownMethod` error
on the very first request it ever sent, before `session/new` was ever
reached. Every other phase's real bug fixes were meaningless for such
a client, since it would never have gotten that far.

Both methods are now `MethodClass::GatewayNative` (no backend process
involved -- this is acpx answering as the ACP agent its own clients
think they're talking to, not anything to do with which backend a
later `session/new` might resolve to). `initialize` returns real
values confirmed against agentclientprotocol.com/protocol/schema's
`InitializeResponse`: `protocolVersion: 1`, `authMethods: []` (acpx-
server's own access control is transport-level -- HTTP bearer token /
WS auth, enforced before a request ever reaches the router -- not an
ACP-level exchange, so there is genuinely no method id to advertise),
and `agentCapabilities` set to the *permissive* end of every flag
(`loadSession: true`, `promptCapabilities.{image,audio,
embeddedContext}: true`, `mcpCapabilities.{http,sse}: true`) rather
than the spec's conservative all-`false` defaults -- deliberate: acpx
is a transparent multiplexing proxy that never inspects, transforms,
or strips prompt content blocks, `mcpServers` transport kinds, or
`session/load` calls, it forwards every one of them verbatim to
whichever backend a later `session/new` resolves to (`classify`'s
`Proxied`/`Hybrid` buckets). Documented honestly as *not* a guarantee
that whichever backend a client's later-chosen profile resolves to
actually supports all of these -- that per-backend truth is only
knowable after `session/new` and already surfaced there via
`_acpx.agentCapabilities` (phase 1). `authenticate` -- since a
compliant client only ever calls it in response to a non-empty
`authMethods`, and acpx's own `initialize` always advertises `[]` --
returns a new `RouterError::NoAuthMethodsAdvertised(Option<String>)`
(carrying whatever `methodId` was requested) rather than silently
succeeding (misrepresenting that real authentication happened) or a
bare method-not-found (misrepresenting `authenticate` itself as
unsupported, when it's the *methodId* that's the problem).

**Second recheck item, verified rather than assumed:** phase 5's
recheck asked whether `permission_policy`/`allow_fs_access`/
`allow_terminal_access`/`auth_method_id` are exposed as first-class
`profiles/*` response fields. Turns out this was already true by
construction since each field was added in phases 3/4/5 -- `Profile`
derives `Serialize` on every `pub` field with no `#[serde(skip)]`
anywhere, so `profiles/create`/`profiles/list`/`profiles/update`'s
responses (which serialize the whole stored `Profile` via
`redact_launch_overrides(serde_json::to_value(&profile))`) already
included all four fields the moment each was added. Nobody had
actually asserted this with a test until now -- `client_initialize_
test.rs`'s fourth test closes that verification gap, not a code gap.
`agentCapabilities` is deliberately *not* added as a static `profiles/*`
field: it's live, per-process runtime information from an actually
spawned backend's real `initialize` response, not profile
configuration that exists before any process is ever spawned -- adding
a fake/stale placeholder value to `Profile` would misrepresent it as
config. It stays exactly where phase 1 put it (`session/new`'s
`_acpx.agentCapabilities`, populated once a backend is actually
running), which is the only point in the request lifecycle where it's
honestly knowable.

New `acpx-core/tests/client_initialize_test.rs` (4 tests): `initialize`
returns real capabilities (not an `UnknownMethod` error); `authenticate`
with any `methodId` gets `RouterError::NoAuthMethodsAdvertised` carrying
that exact id back; `session/new` still works without ever calling
`initialize` first (phase 6 is additive, not a new hard prerequisite --
every native/unmanaged-mode client in this workspace's pre-existing
tests never called it); and the `profiles/*` field-exposure check
described above, via both `profiles/create`'s own response and a
separate `profiles/list` call. Plus a new test in `acpx-server/tests/
binary_self_test.rs`, `real_binary_answers_the_client_facing_
initialize_and_authenticate_handshake` -- the strongest proof available
in this workspace, since it drives the real, already-compiled
`acpx-server` binary over a real HTTP connection exactly the way a real
ACP editor would: `initialize` first, `authenticate` with an arbitrary
method id second (asserting a real JSON-RPC error body, HTTP 200 per
this transport's existing error-envelope convention -- not a hang, not
a connection drop), then `session/list` to prove the real process is
still fully usable afterward.

Workspace test count after this addition: **163 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. This phase closes every item on phase 5's recheck list. Re-deriving
   from scratch against the full spec surface (agentclientprotocol.com/
   protocol/schema) rather than just prior phases' leftover lists: ACP
   also defines `session/cancel` (already `Proxied`, forwarded
   verbatim -- see `classify`), image/audio/resource `ContentBlock`
   variants in prompts (acpx never inspects prompt content at all,
   forwards every block type verbatim -- consistent with this phase's
   permissive `promptCapabilities` declaration), and `_meta` fields on
   most request/response shapes (acpx forwards `params`/`result`
   objects verbatim in every `Proxied`/`Hybrid` path, so any `_meta` a
   real client or backend sends already survives untouched -- not
   something acpx needs to special-case). No further gaps identified
   in the wire-protocol surface itself as of this phase.
2. The live-interactive-decision gap from every prior phase's recheck
   is unaffected by this phase (it's about mid-call backend-initiated
   requests, not the opening handshake) -- still the honest, tracked,
   not-attempted-in-this-series residual.
3. What remains genuinely open is no longer "is a wire-protocol method
   missing" but operational/hardening concerns one level up: TLS (see
   `05-open-risks.md`, unchanged by this series), and whether acpx's
   own `initialize` response should ever become backend-dependent
   (e.g. narrower `agentCapabilities` for a client that's already told
   acpx which profile/backend it intends to use via some future
   pre-`session/new` signal) rather than the fixed, permissive-proxy
   values this phase ships -- not attempted, no evidence yet that any
   real client needs it (`_acpx.agentCapabilities` on `session/new`
   already covers the "what does my actual backend support" question
   once a session exists).

## ACP compatibility hardening, phase 7 -- real `session/cancel`

Re-deriving the ACP spec surface from scratch for phase 6's "no further
gaps identified" recheck turned out to be premature: `session/cancel`
is one of exactly **four** methods the spec calls out as a baseline
MUST for every agent (`session/new`, `session/prompt`, `session/cancel`,
`session/update`), and this workspace had **zero** tests exercising it
at all before this phase, across ~163 pre-existing tests. Real schema
(agentclientprotocol.com/protocol/schema): `CancelNotification` is a
client-sent *notification* -- no `id`, and the spec is explicit that
the agent "MUST" eventually resolve the turn it interrupts by having
the *original* `session/prompt` call return `stopReason: "cancelled"`,
not by replying to the cancel itself.

**Three real bugs found and fixed, not one:**
1. Every `Proxied` method (including `session/cancel`, before this
   phase) unconditionally required an `id`
   (`RouterError::MissingId` otherwise). A spec-compliant client
   sending a true notification (no `id` at all) would have been
   rejected before the request ever reached a backend.
2. The generic proxied path blocks on `read_matching_response` waiting
   for a reply carrying the forwarded request's own id. A
   spec-compliant backend never replies to `session/cancel` directly --
   routing it through that path would hang forever against any
   correctly-implemented backend. Same category of bug as phase 2's
   `session/request_permission` deadlock fix, mirrored in the opposite
   direction: there, an agent-initiated *request* was mistaken for a
   notification; here, a client-sent *notification* was mistaken for a
   request awaiting a reply.
3. **The deepest one, and the reason this isn't just a shape fix:**
   even with (1)/(2) fixed, routing `session/cancel` through the same
   per-process lock (`SharedBackendProcess`'s
   `Arc<Mutex<BackendProcess>>`) every other proxied method uses would
   make cancellation practically useless. A `session/prompt` call
   already in flight against that exact backend process holds that
   lock for its *entire* duration (the whole point of the "real
   multi-agent concurrency" design from earlier in this project) -- a
   cancel routed through it could only ever be delivered *after* the
   very call it's meant to interrupt has already finished, at which
   point cancelling is moot. Confirmed by inspection of `Supervisor::
   ensure_running`'s own liveness check (`self.running.get(agent_id)
   .lock().await`), which blocks on that exact same per-process lock
   before a second call against a busy agent can even proceed.

Fixed by giving `session/cancel` a genuinely independent write path.
`acpx_conductor::process::BackendProcess::writer` changed from a bare
`FramedWriter` to `Arc<tokio::sync::Mutex<FramedWriter>>` -- every
pre-existing write call site (7 of them across `router.rs`, plus one in
`e2e_single_agent_test.rs`) now does one extra `.lock().await` on this
small, fast, independent mutex, mechanically unchanged in behavior.
`Supervisor` gained a second bookkeeping map, `write_handles`, populated
with a clone of this exact `Arc` *at spawn time* -- before the fresh
`BackendProcess` is ever wrapped in its own outer per-process
`Arc<Mutex<..>>` and handed out -- so `Supervisor::cancel_writer(
agent_id)` can hand out a working writer handle via only `Supervisor`'s
own (already brief, already-existing) lock, never touching the
per-process lock at all. `Router::dispatch_session_cancel`/the free
`dispatch_session_cancel_shared` function (mirroring the existing
`dispatch_proxied`/`dispatch_proxied_shared` split) resolve the
session, build the real ACP notification shape verbatim (`{jsonrpc,
method, params: {sessionId}}` -- deliberately no `id` key, regardless
of whatever shape the client's own call used), write it through
`cancel_writer`, and reply to the *client* immediately (echoing the
client's own `id`, or `null` if it sent a true notification) without
ever blocking on any backend reply. `classify` still routes
`session/cancel` into the `Proxied` bucket (unchanged, so no
`MethodClass` shape broke); `dispatch_proxied`/`dispatch_shared` now
special-case it to the new path before the generic `id`-requiring code
runs. A session whose backend was never spawned (or was `stop`ped) gets
a benign no-op (`cancel_writer` returns `None`) rather than an error --
nothing is in flight to interrupt.

New `acpx-core/tests/session_cancel_test.rs` (3 tests, non-shared
`Router::dispatch`, no real concurrency needed to prove these): a true
notification (no `id` at all) completes without hanging even though the
stand-in backend never replies, and the captured raw line the backend
actually received has the real ACP shape (rewritten `sessionId`, no
`id` key); a client that *does* attach an `id` still gets it echoed
back in acpx's own reply, but the backend still receives the real
id-less shape regardless; an unknown session gets a clear error. New
`acpx-server/tests/session_cancel_concurrency_test.rs` (1 test) proves
bug 3's fix specifically, against the real HTTP transport and the real
concurrent (`dispatch_shared`) dispatch path: fires `session/prompt`
(1.5s simulated latency) on its own task, waits 300ms to be sure it's
genuinely in flight and holding the per-process lock, then measures
`session/cancel` against the *same* session -- asserts it completes in
under 700ms (versus the ~1.2s remaining latency a lock-serialized
delivery would take), that the in-flight prompt still finishes normally
afterward, and that the real notification's raw bytes landed on the
backend's stdin (captured to a temp file) during the prompt's own sleep
window, not after.

Workspace test count after this addition: **167 passed, 0 failed, 3
ignored**, `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. `session/update`, the fourth baseline-MUST method, was already
   real (delivered via `read_matching_response`'s notification
   aggregation, `_acpx.updates` -- see the "real ACP content delivery"
   section of this doc from earlier in the project). All four
   baseline-MUST methods (`session/new`, `session/prompt`,
   `session/cancel`, `session/update`) are now genuinely implemented,
   not just present in `classify`'s dispatch table.
2. `Supervisor::ensure_running`'s liveness check blocking the whole
   `Router`-level lock whenever a *second* concurrent call (of any
   kind, not just `session/cancel`) targets an agent that already has
   one in flight -- noted above as how bug 3 was confirmed -- is a
   real, adjacent concurrency/scalability concern, but it's not itself
   an ACP wire-protocol compatibility gap (no client observes a
   protocol violation from it, only added latency for unrelated
   agents' concurrent requests while it's blocked). Deliberately left
   unfixed in this phase, which is scoped to ACP compatibility per this
   project's current directive -- tracked here honestly rather than
   silently bundled in or silently ignored.
3. The live-interactive-decision gap from every prior phase's recheck
   is unaffected by this phase (still about mid-call backend-initiated
   requests) -- still the honest, tracked, not-attempted-in-this-series
   residual.

## 2026-07-13 -- ACP compatibility phase 8: real `session/load` rehydration across a gateway restart

**Directive:** continuation of the phase-by-phase ACP compatibility
hardening series ("go ahead and phase wise fix there all compatibility
one by one and recheck if any acp compatibility is missing at each
stage"). Picked up phase 7's recheck list, specifically: "Whether
`session/load` (resuming a persisted session) is genuinely implemented
end-to-end or just classified -- verify actual backend round-trip
behavior, not just dispatch routing."

**Real, previously-undiscovered gap found and fixed:** `session/load`
was classified `Proxied` (phase 1) and generically forwarded like every
other session-scoped method, but `dispatch_proxied`/`dispatch_proxied_
shared` resolved the caller's `sessionId` *only* against the in-memory
`SessionRegistry`. That registry is wiped clean on every acpx process
restart. `session/load` exists in the real ACP spec specifically so a
client can resume a session it learned about through some channel other
than "I just called `session/new` in this exact process's lifetime" --
overwhelmingly, reconnecting after the agent process restarted. Before
this phase, every `session/load` call against a gateway session id from
a previous acpx process lifetime failed with `UnknownSession`, even
though acpx's own sqlite (`ACPX_DB_PATH`, `persistence::sessions`
table) already had a durable row proving the session existed and which
real backend/profile it belonged to -- the exact data needed to recover
was sitting right there, unused. This made acpx's `session/load` support
strictly *less* capable than a real single-agent ACP agent's, defeating
the entire reason the method exists as distinct from `session/new`.

**Fix:** `Router::rehydrate_session` (new, `acpx-core/src/router.rs`) --
invoked as a fallback by both `dispatch_proxied` and `dispatch_proxied_
shared` only when the in-memory lookup misses, and only for `session/
load`/`session/resume` specifically (every other `Proxied` method still
requires a live in-process session and correctly errors `UnknownSession`
otherwise -- those aren't resumption calls, so silently reviving one
from a stale row on, say, a typo'd `session/prompt` call would paper
over a real client bug). It reads the row from `PersistenceStore::
get_session`, reconstructs a `SessionEntry`, and -- **the second, less
obvious half of this bug, only caught by testing against a genuinely
separate second process** -- re-runs `resolve_profile` for the
persisted `profile_name` before returning. Without that second step the
fix half-failed: the session row resolves fine, but the *new* process's
`Supervisor` has never registered a `SpawnSpec` for that profile's
`profile:{name}` key (that registration is normally a `session/new`-time
side effect), so `ensure_running` errored "no spawn spec registered for
agent profile:...". `resolve_profile` is idempotent (same code path
`session/new` already calls every time), so re-running it here is safe.
`SessionRegistry::insert` (new) re-inserts the recovered entry under the
*original* gateway session id (not a freshly minted one -- the whole
point is the client already knows this id) so it's usable for ordinary
calls afterward, not just the one `session/load` request. Two new
`RouterError` variants distinguish "no persistence configured at all"
(`SessionNotPersisted`) from "persistence configured but the lookup
itself failed" (`SessionRehydrationFailed`) from the pre-existing
`UnknownSession` (genuinely never existed / not a resumption method) --
three different failure modes worth telling apart for whoever reads the
error.

**Test:** `acpx-server/tests/real_ambient_multi_agent_test.rs`'s new
`ambient_claude_session_load_survives_a_real_gateway_restart` (opt-in,
`ACPX_LIVE_TEST_AMBIENT=1`, same real-ambient-auth-`claude`-CLI pattern
as the rest of that file) is the real thing end to end, not a
simulation: spawns one real `acpx-server` process against a real sqlite
file, creates a real `claude-agent-acp` session via a profile, sends one
real billed `session/prompt` turn, closes the session, **kills that
whole process**, spawns a **second, fully independent** `acpx-server`
process against the *same* sqlite file (fresh empty `SessionRegistry`,
fresh empty `Supervisor` -- nothing carries over except the file), and
calls `session/load` against it using the *first* process's gateway
session id (never re-declared via `session/new` in the second process).
Proves, all against real subprocesses: (1) the rehydration lookup finds
the row, (2) it correctly resolves back to `claude-acp`/the right
profile so the second process spawns a fresh real adapter for it, (3)
the forwarded backend session id is right (the real adapter accepts it,
no "Session not found"), (4) `session/set_mode` against that same
rehydrated session works too (reusing the same live session rather than
a second billed test -- picks a real, non-default `modeId` straight out
of this exact adapter build's own `session/load` response, never
hardcoded -- zero real-backend coverage of `session/set_mode` existed
anywhere in this workspace before this phase either), and (5) the
gateway session id is reusable afterward in the *new* process for a
real follow-up `session/prompt` turn (the strongest proof -- rehydration
didn't just answer one `session/load` call, it genuinely re-registered a
working session). Also confirmed empirically and documented in the
test's own comments: the real `LoadSessionResponse` schema (agentclient
protocol.com/protocol/schema) has no `sessionId` field at all, so the
test doesn't assert identity-consistency of `claude-agent-acp`'s own
non-standard extra `sessionId` key in that response (acpx forwards it
verbatim, transparent-proxy style, same as any other field it doesn't
special-case); and that this specific adapter build emitted zero
`session/update` history-replay notifications for this one-turn session
(adapter-internal detail, not asserted on either way).

`spawn_real_server_with_db` (new helper in the same test file) wires
`ACPX_DB_PATH` through to the real binary for both processes; `spawn_
real_server` (pre-existing, used by every other test in the file)
becomes a thin wrapper delegating to it with `db_path: None`, unchanged
behavior for every caller of the old function.

Workspace test count after this phase: **167 passed, 0 failed, 4
ignored** (the 4th ignored is this phase's new live test; the other 3
are the pre-existing real-CLI-auth-needed tests), `cargo fmt --all
--check` and `cargo build --workspace --tests` both clean. The new live
test itself was run for real against this machine's ambient `claude`
CLI auth (`ACPX_LIVE_TEST_AMBIENT=1 cargo test ... -- --ignored`) and
passes.

**Recheck against the full ACP spec surface after this phase:**
1. `session/set_mode`/`session/set_config_option`'s real backend
   forwarding is now confirmed correct post-phases-5-7's `BackendCall
   Policy`/writer-lock refactors (per phase 7's recheck item) -- this
   phase's live test exercises `session/set_mode` for real, and `real_
   claude_multi_agent_test.rs`/`real_ambient_multi_agent_test.rs`
   already exercised `session/set_config_option` for real before this
   phase. Both closed.
2. `session/resume` shares `rehydrate_session`'s fallback path with
   `session/load` (both are spec-defined resumption methods with the
   same "must survive not having a live in-process registry entry"
   requirement) but has **not** been exercised by a real end-to-end test
   the way `session/load` now has in this phase -- `claude-agent-acp`
   does implement `resumeSession` (confirmed by reading its own compiled
   `dist/acp-agent.js` in this phase's investigation), so this is
   concretely testable, just not yet tested. Tracked as this phase's
   most direct next step, not a "someday" item.
3. `session/list`'s real ACP spec shape (per agentclientprotocol.com:
   `{sessionId, cwd, title, updatedAt}` per entry, cursor-paginated,
   gated behind an agent-advertised `listSessions` capability) does
   **not** match what acpx currently answers for that same method name:
   acpx's `dispatch_native`'s `"session/list"` arm (`router.rs`) returns
   its own gateway-only shape (`{sessionId, agentId}`, no pagination, no
   capability gate) sourced from the in-memory `SessionRegistry`, not
   from any real backend agent's own session list at all. Found during
   this phase's investigation but **not fixed** -- this is a real,
   pre-existing naming collision between an acpx-native admin/management
   concept (list what this multiplexing gateway itself currently has
   open, across every backend) and the real ACP wire method of the same
   name (ask **the one connected agent** what sessions **it** has
   persisted), and fixing it cleanly is an architectural decision (does
   acpx keep the gateway-scoped meaning under this name and accept the
   spec-shape mismatch, forward it to the profile's backend instead and
   lose the multi-agent aggregate view, or rename the gateway-native one
   to something like `acpx/sessions/list` and make `session/list` a real
   `Proxied` per-backend call) -- deliberately not decided unilaterally
   in this phase, tracked here honestly rather than silently left as an
   unlabeled inconsistency. Also not advertised in acpx's own `
   initialize` response's `agentCapabilities` (no `listSessions` key at
   all), which is at least internally consistent with *not* claiming
   spec compliance for it.
4. `session/delete` and `logout` (both promoted to stable in the ACP
   spec per this phase's research, alongside `session/set_config_option`
   /`session/set_mode`) are not classified anywhere in acpx's `classify`
   function at all -- unlike phase 6's `initialize`/`authenticate` gap,
   these fall through to `MethodClass::Unknown` today. `claude-agent-acp`
   does implement `deleteSession` (confirmed in the same `dist/acp-
   agent.js` read during this phase). Not fixed in this phase (found
   late, budget-constrained) -- concrete next step.
5. `session/fork` (unstable per spec, but `claude-agent-acp` does
   implement `unstable_forkSession`) and `elicitation/create`/
   `elicitation/complete` (unstable) are not classified either. Lower
   priority than (3)/(4) since they're explicitly unstable in the spec
   itself, but worth a follow-up recheck once the stable-method gaps
   above are closed.
6. `ContentBlock` variant (image/audio/resource) passthrough and `_meta`
   field passthrough in `session/prompt` -- both still claimed-but-
   unverified-by-an-explicit-test per phase 7's recheck list. Not
   reached this phase; still open.

## 2026-07-13 -- ACP compatibility phase 9: `session/delete` and `logout`, real v1 schema fetched directly

**Directive:** continuation of the same phase-by-phase series, picking
up phase 8's recheck items 3-4. Given phase 8 surfaced conflicting
secondary-source claims about which methods are actually stable
(`session/delete` described as both "still an RFD" and "promoted to
stable" by different summaries), this phase fetched the real, current
`schema/v1/schema.json` directly from `agentclientprotocol/agent-
client-protocol` on GitHub rather than trusting search-summarized
secondary sources any further -- the authoritative source for every
claim below.

**Confirmed from the real schema (`x-method`/`x-side` fields):**
`session/delete` (`DeleteSessionRequest{sessionId}`/`DeleteSessionResponse`,
`x-side: agent`) and `logout` (`LogoutRequest{}`/`LogoutResponse{}`,
`x-side: agent`, no `sessionId` -- connection-scoped) are both real,
stable v1 methods. Neither was classified anywhere in `classify` before
this phase -- both fell through to `MethodClass::Unknown`, same category
of gap as phase 6's pre-fix `initialize`/`authenticate`. `claude-agent-
acp`'s own compiled `dist/acp-agent.js` was read directly in this phase
and confirmed to implement `deleteSession` for real, so this was a
concretely exercisable gap, not theoretical.

**Fix:**
1. `session/delete` classified `Proxied` (session-scoped, forwards
   verbatim like `session/close`) and added to `rehydrate_session`'s
   allowlist alongside `session/load`/`session/resume` from phase 8 --
   deleting a session a client knows about from a previous acpx process
   lifetime is exactly as legitimate as loading/resuming one.
2. `logout` classified `GatewayNative`, not `Proxied` -- it has no
   `sessionId`, so in acpx's multi-backend gateway there is no single
   unambiguous backend it could target, unlike a real single-agent ACP
   agent with exactly one connection. `dispatch_native`'s new `"logout"`
   arm errors with a new, specific `RouterError::LogoutNotSupported`,
   mirroring phase 6's `authenticate`/`NoAuthMethodsAdvertised` precedent
   exactly: acpx's own gateway-level auth is transport-level (HTTP
   bearer/WS), not ACP-level, so there is genuinely no authenticated
   state at the gateway layer for `logout` to terminate; forwarding it
   to one arbitrary backend among potentially many active profiles would
   be actively misleading, and silently no-op-succeeding would
   misrepresent that something real happened.
3. Fetching the real schema also surfaced that acpx's own client-facing
   `initialize` response's `agentCapabilities` was missing
   `sessionCapabilities` entirely (a whole real v1 sub-object, distinct
   from the already-correct top-level `loadSession` flag). Added
   `sessionCapabilities: {close: {}, delete: {}, resume: {}}` -- honest,
   since all three are genuinely `Proxied` end to end. Deliberately
   **excludes** `list`: acpx's own `session/list` handler answers from
   its own gateway-scoped `SessionRegistry` (`{sessionId, agentId}` per
   entry, no pagination, no per-backend `SessionInfo` shape), not the
   real per-backend, cursor-paginated `session/list` schema -- see
   finding 4 below; advertising `list: {}` would be a false spec-
   compliance claim about a method whose current shape is a known,
   tracked divergence. Also excludes `additionalDirectories` (acpx
   forwards whatever a client sends but never itself validates/acts on
   it, so there's no acpx-level capability claim to make either way) and
   `auth.logout` (matches finding 2 -- not actually supported).

**Tests:** `acpx-core/src/router.rs`'s `classifies_phase_9_stable_
methods` (classification only); new `acpx-core/tests/session_load_
rehydration_test.rs` (4 tests) -- deterministic, no real subprocess/
billing, using `session/close` (which phase 7 made evict the in-memory
registry while leaving the durable row alone) as a cheap stand-in for "a
restart happened": `session/load` rehydrates after close and the
session is genuinely reusable for a real follow-up `session/prompt`
afterward; `session/delete` rehydrates the same way; `session/prompt`
(an ordinary, non-resumption `Proxied` method) does **not** rehydrate
even with a matching persisted row present, proving the allowlist scope
is real and enforced, not just documented; `session/load` with no
`Router::with_persistence` configured at all fails with the specific
`SessionNotPersisted` rather than a generic/misleading error.
`acpx-core/tests/client_initialize_test.rs` gained assertions on the new
`sessionCapabilities` fields (including asserting `list` is absent) and
a new `logout_is_refused_with_a_clear_error_since_no_logout_capability_
is_advertised` test.

Workspace test count after this phase: **173 passed, 0 failed, 4
ignored**, `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. `session/resume` still shares the rehydration path but remains
   untested by a real end-to-end test (carried over from phase 8,
   unchanged -- still the most direct concrete next step).
2. `session/list`'s real-vs-gateway-native shape mismatch (finding 3
   above) remains unfixed -- now at least *honestly* unadvertised in
   `initialize` rather than silently inconsistent, but the underlying
   architectural decision (rename the gateway-native concept vs. make
   `session/list` a real per-backend `Proxied` call vs. accept the
   divergence permanently) is still open. Carried over from phase 8.
3. `session/fork` (unstable, but `claude-agent-acp` implements
   `unstable_forkSession`) and `elicitation/create`/`elicitation/
   complete` (unstable) remain unclassified -- confirmed via the real
   schema fetched this phase that these are genuinely absent from the
   *stable* v1 schema (only present in an unstable/v2 schema this phase
   didn't fetch), so lower priority than any remaining stable-method gap
   by the spec's own stability contract. Worth a dedicated recheck once
   stable-surface gaps are exhausted.
4. `ContentBlock` variant (image/audio/resource) passthrough and `_meta`
   field passthrough in `session/prompt` -- still open, carried over
   unchanged from phases 7/8.
5. Not yet re-verified this phase: whether `terminal/*`'s and `fs/*`'s
   real schemas (phases 3/4) still match the v1 schema fetched this
   phase exactly -- this phase's fetch was scoped to `session/*`/
   `logout`/`agentCapabilities`; a full field-by-field diff against
   every previously-implemented method using this same authoritative
   source (rather than the secondary summaries earlier phases relied on)
   is a good candidate for a future phase, purely as a confidence check
   rather than because any specific discrepancy is suspected.

## 2026-07-13 -- ACP compatibility phase 10: `terminal/output`'s missing required `truncated` field

**Directive:** continuation of the same series, acting on phase 9's
recheck item 5 ("re-diff `terminal/*`/`fs/*` against the real schema").
Diffed every `terminal/*`/`fs/*` request/response's real property list
(from the same `schema/v1/schema.json` fetched in phase 9) against
`acpx-core::router::handle_terminal_request`/`handle_fs_request`'s
actual JSON shapes, field by field.

**Real, previously-undiscovered gap found:** `TerminalOutputResponse`'s
real schema marks `truncated` (boolean, "Whether the output was
truncated due to byte limits") as a **required** property (`"required":
["output", "truncated"]`) -- not optional. `acpx-conductor::terminal::
TerminalHandle`'s `output_byte_limit` truncation (drain-from-the-front
once the captured buffer exceeds the limit, phase 4) worked correctly,
but nothing ever recorded *that* truncation had happened, and `acpx-
core`'s `terminal/output` handler never emitted the field at all --
every `terminal/output` reply acpx ever sent back to a real backend
agent was missing a spec-required field. A strict, schema-validating
deserializer on the backend side could reject the reply outright; even a
lenient one would have no way to distinguish "the agent captured
everything the command printed" from "the agent's buffer silently
dropped some of the oldest output" -- which matters concretely for a
long-running command an agent is polling `terminal/output` against
repeatedly (a build log, a dev server, etc.).

**Fix:** `terminal::Shared` gained a `truncated: bool` field, set (once
set, sticky -- matches the field's own "was truncated" semantics rather
than "is currently truncated relative to this exact byte offset")
whenever `spawn_capture_task`'s drain-on-overflow branch actually fires.
`TerminalHandle::output()`'s return type changed from `(Vec<u8>,
Option<TerminalExitStatus>)` to `(Vec<u8>, bool,
Option<TerminalExitStatus>)`; `handle_terminal_request`'s `"terminal/
output"` arm now includes `"truncated": bool` in the JSON-RPC result.

**Tests:** `acpx-conductor/src/terminal.rs`'s two existing unit tests
(`captures_output_and_exit_status`, `output_byte_limit_truncates_from_
the_front`) gained assertions on the new field (`false`/`true`
respectively -- the second one specifically proves the byte-limit-
exceeded path sets it). `acpx-core/tests/terminal_request_test.rs`'s
existing real-subprocess-through-the-router integration test gained an
assertion that a real `terminal/output` reply (no byte limit configured)
carries `"truncated": false` explicitly, rather than the field being
absent.

Workspace test count after this phase: **173 passed, 0 failed, 4
ignored** (no new tests added, only new assertions in existing ones, so
the count is unchanged from phase 9), `cargo fmt --all --check` and
`cargo build --workspace --tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. The rest of the `terminal/*`/`fs/*` field-by-field diff (`terminal/
   create`, `terminal/wait_for_exit`, `terminal/kill`, `terminal/
   release`, `fs/read_text_file`, `fs/write_text_file`) found no further
   discrepancies against the real schema -- every other field name/
   optionality already matched exactly. This closes phase 9's recheck
   item 5 as "checked, one real gap found and fixed," not "checked,
   nothing found" -- worth noting given phase 6's recheck once declared
   "no further gaps" only for phase 7 to find `session/cancel` entirely
   missing; this phase's finding is the same category of lesson playing
   out again on a narrower surface.
2. `session/resume` (phase 8/9 carryover) still shares the rehydration
   path but remains untested end to end by a real adapter.
3. `session/list`'s shape mismatch (phase 8/9 carryover) remains an open
   architectural decision.
4. `ContentBlock`/`_meta` passthrough in `session/prompt` (phase 7/8/9
   carryover) still open -- given this phase's finding, worth explicitly
   re-stating: "acpx forwards it verbatim" is a design claim, not yet a
   tested one, and this phase is a concrete reminder that untested
   claims in this codebase have had real gaps hiding behind them before.

## 2026-07-13 -- ACP compatibility phase 11: `session/prompt`'s `ContentBlock`/`_meta` passthrough, tested for real

**Directive:** continuation of the same series, closing phase 7's
recheck item (carried, untested, through phases 8/9/10): "Any
`ContentBlock` variant (image/audio/resource) plumbing gaps in `session/
prompt` -- acpx claims to forward verbatim; confirm no accidental
transformation." Phase 10's finding (a real gap hiding behind exactly
this kind of untested "forwards it verbatim" claim, on a neighboring
method) made leaving this one unverified any longer indefensible.

**Result: no code gap found this time -- the passthrough claim held.**
New `acpx-core/tests/prompt_content_passthrough_test.rs` sends a real
`session/prompt` through a real (in-process, `Router::dispatch`,
non-shared) dispatch with one of every real `ContentBlock` variant from
the schema fetched in phase 9 (`text`, `image`, `audio`, `resource_link`,
`resource`) in a single `prompt` array, each carrying its own
`annotations`/`_meta` where the real schema allows it, plus a top-level
`PromptRequest._meta`. A stand-in backend captures the exact raw line it
receives; the test deserializes it and asserts the captured `params.
prompt` deep-equals the original array exactly (`serde_json::Value`
structural equality, not just a spot-check) and `params._meta` likewise,
with only `sessionId` (the one field the router is *supposed* to
rewrite, gateway id -> backend id) differing. All pass -- `dispatch_
proxied`'s generic `params["sessionId"] = ...` rewrite-in-place, with
everything else forwarded via the same `serde_json::Value` untouched,
really does mean untouched, down to nested per-block `_meta`/
`annotations`.

Workspace test count after this phase: **174 passed, 0 failed, 4
ignored**, `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. `session/resume` (carried since phase 8) still shares the
   rehydration path but remains untested end to end by a real adapter --
   now the single most direct, concrete, and long-carried next step in
   this list.
2. `session/list`'s shape mismatch (carried since phase 8) remains an
   open architectural decision, not a "next phase" item by itself
   (deliberately deferred pending a decision, not forgotten).
3. This phase closes the last item that had been carried unaddressed
   across three consecutive phase recheck lists (7, 8, 9, 10) -- worth
   noting for whoever continues this series: don't let a "still open,
   carried over" item ride indefinitely just because it sounds lower-risk
   than the phase's headline finding; phase 10 is direct proof that
   untested verbatim-forwarding claims are exactly where gaps hide.

## 2026-07-13 -- ACP compatibility phase 12: `session/resume` verified end to end against a real gateway restart

**Directive:** continuation of the same series, closing phase 8's
recheck item (carried, untested, through phases 9/10/11): "`session/
resume` still shares the rehydration path but remains untested end to
end by a real adapter." Phase 11's lesson (don't let untested "shares
the same code path" claims ride indefinitely) made this the direct next
step.

**Real schema check first** (`ResumeSessionRequest`/`ResumeSessionResponse`
/`SessionResumeCapabilities`, `/tmp/acp_schema.json` from phase 9):
`session/resume` is a real, stable v1 method, request shape nearly
identical to `session/load`'s (`sessionId`/`cwd`/`additionalDirectories`/
`mcpServers`/`_meta`), response deliberately lighter (`modes`/
`configOptions`/`_meta` only, explicitly **no history replay** --
"Resumes an existing session without returning previous messages...
useful for agents that can resume sessions but don't implement full
session loading"). `acpx-core::router::classify` already groups it with
`session/load` under `Proxied`, and `rehydrate_session`'s restart-
survival fallback (phase 8) already lists `"session/resume"` explicitly
alongside `"session/load"`/`"session/delete"` -- so, per the code, this
should already work; the open question was purely "has this actually
been exercised against a real backend adapter," not "is there known-
missing code."

**Result: no code gap found -- the shared-path claim held on first real
run.** New test `ambient_claude_session_resume_survives_a_real_gateway_
restart` in `acpx-server/tests/real_ambient_multi_agent_test.rs` mirrors
phase 8's `session/load` restart test almost exactly (spawn a real
`acpx-server` process against a real sqlite file, create a real `claude-
agent-acp` session, one billed prompt turn, kill the whole process, spawn
a second independent process against the same sqlite file, re-declare the
profile) but deliberately diverges in two ways to keep the two tests
testing genuinely different things rather than duplicating one another:
(1) the first process's session is **never closed** before the process is
killed (exercises the more realistic "acpx died mid-session" scenario,
vs. the `session/load` test's tidier "client closed it first" one), and
(2) the second process calls **only** `session/resume` -- no `session/
load` call anywhere in this test -- and then drives a real follow-up
billed prompt turn through the resumed session to prove it was genuinely
re-registered with the second process's own `Supervisor`, not just that
the RPC call itself returned successfully. Ran for real (not simulated)
via `ACPX_LIVE_TEST_AMBIENT=1`: passed on the first attempt, no fix
needed.

Workspace test count after this phase: **174 passed, 0 failed, 4
ignored** (unchanged -- the new test is itself `#[ignore]`d/opt-in, like
the rest of this file's real-ambient tests, so it doesn't move the
default-run count; it was run manually with `--ignored` and confirmed
passing against real ambient `claude` auth on this machine).
`cargo fmt --all --check` and `cargo build --workspace --tests` both
clean.

**Recheck against the full ACP spec surface after this phase:**
1. `session/resume` (carried since phase 8) is now closed -- verified for
   real, no gap. This was the last remaining item in the "shares a code
   path, never independently confirmed against a real backend" category
   that this series had been tracking since phase 8.
2. `session/list`'s shape mismatch (carried since phase 8) remains the
   one open architectural item in this series. Decision made this phase
   rather than deferred further (see below).
3. `session/fork` (unstable) and `elicitation/create`/`elicitation/
   complete` (unstable) remain out of scope per the stable v1 schema's
   own stability contract, unchanged from phase 9's finding.

**`session/list` architectural decision, made this phase:** three options
were on the table since phase 8/9 (keep the gateway-scoped meaning as-is;
forward to a single backend and lose the multi-agent aggregate view; or
split the two concepts under different method names). The objective this
whole series serves is explicit that ACP spec compatibility and acpx's
multiplex-management value (the reason a gateway exists instead of a
client talking to one backend adapter directly) must **both** hold, not
trade one for the other -- so option 2 (silently reinterpreting the real
`session/list` as single-backend-only) was rejected as a regression of
acpx's core purpose, and option 1 (leaving it gateway-scoped forever)
was rejected as leaving a real spec method permanently non-compliant
when a real, non-breaking fix exists. **Chosen: split.** This is tracked
as the concrete next phase (13) rather than done opportunistically here,
since it touches wire-visible behavior (a real, spec-shaped, per-backend
`Proxied` `session/list`) and deserves its own build/test/fmt/commit
cycle like every other phase in this series.

## 2026-07-13 -- ACP compatibility phase 13: `session/list` split into a real per-backend proxy + acpx's own gateway aggregate

**Directive:** continuation of the same series, executing phase 12's
architectural decision (split, not rename-and-lose-either-half): both
ACP spec compatibility for `session/list` and acpx's multi-agent
aggregate view (the entire reason a multiplexing gateway exists over a
client talking to one backend adapter directly) had to keep holding,
not trade one for the other.

**Design.** `session/list` is now dual-mode on the *same* method name,
distinguished by the same `_acpx` extension convention `session/new`
already established for `_acpx.profile` (managed-mode backend
selection):
- **No `_acpx.profile`/`_acpx.agentId` in `params`:** unchanged
  behavior in spirit -- acpx's own gateway-scoped aggregate across every
  backend it currently manages, from the in-memory `SessionRegistry`.
  No real single backend could ever honestly answer this question on
  its own. Gained one new field this phase: `cwd` (see below).
- **With `_acpx.profile: <name>` or `_acpx.agentId: <id>`:** a real,
  spec-shaped `Proxied` forward to that one specific backend's own
  `session/list` (params minus `_acpx`), with every returned
  `SessionInfo.sessionId` translated from the backend's native id into
  a gateway id before it reaches the client.

**Why the translation step is the real substance of this phase, not a
detail.** The real `ListSessionsRequest`/`ListSessionsResponse`/
`SessionInfo` schemas (fetched fresh, `/tmp/acp_schema.json`) have no
`sessionId` field on the *request* at all -- `session/list` isn't scoped
to an existing session the way most other ACP methods are, it's a
connection-level query answered by whichever one backend agent
connection the caller has. Forwarding the *response*'s raw
backend-native `SessionInfo.sessionId`s straight through, unmodified,
would have hand the client ids it could never use against any other
acpx method again (`session/load`, `session/prompt`, ... all require a
*gateway* id) -- technically schema-shaped, but practically useless
through a proxy, and a dead end for `session/load`'s own restart-
survival story (phase 8/9/12): an untranslated id has no
`SessionRegistry`/persisted-`SessionRecord` row for `rehydrate_session`
to find. New `Router::translate_or_register_backend_session` (paired
with new `SessionRegistry::find_by_backend`, a reverse `(agent_id,
backend_session_id) -> gateway_id` lookup) reuses an already-known
gateway id when this exact backend session was already registered
(typically because acpx itself opened it via `session/new` earlier in
this process's lifetime), or mints and registers a fresh one on the
spot for a genuinely new discovery -- e.g. a session `claude-agent-acp`
reports from the real Claude Code SDK's own on-disk history that
predates or falls outside this exact acpx process's own `session/new`
calls. From that point on the discovered id is a first-class gateway
session, `session/load`-able exactly like any other. Proved concretely,
not just asserted: `session_list_real_test.rs`'s `...proxies_to_the_
real_backend_and_translates_ids` calls `session/close` on a freshly-
discovered id afterward and requires it to succeed (a bare opaque
string that was never actually registered would fail `UnknownSession`).

**Second real gap closed in passing: `SessionInfo.cwd` is a *required*
field**, and nothing in `SessionRegistry` tracked a session's `cwd`
before this phase -- so even acpx's own gateway-scoped aggregate could
never have honestly included it (an omission that would have been a
second, independent reason `sessionCapabilities.list` stayed
unadvertised even after the real-proxy half was built). New
`SessionEntry::cwd: Option<String>`, populated from `session/new`'s own
`params.cwd` (both `dispatch_session_new` and its `_shared` twin) and
from a real backend's `SessionInfo.cwd` when a session is discovered via
the real `session/list` path. **Known, tracked limitation, not silently
dropped:** the sqlite `sessions` table (`SessionRecord`) still doesn't
carry `cwd`, so a session rehydrated via `rehydrate_session` (`session/
load`/`session/resume`/`session/delete` surviving a restart) reports
`cwd: null` in the aggregate view until a future phase extends that
table -- an honest gap, not a regression this phase introduced (`cwd`
was never tracked anywhere before this phase).

**`initialize`'s `agentCapabilities.sessionCapabilities.list`** is now
advertised (`{}`), closing the divergence phases 9-12 all deliberately
left unadvertised and tracked. Honest in the same qualified sense
`loadSession`/`promptCapabilities` already are: a real claim that the
method can be genuinely spec-conformant, not that every unqualified
call is (an unqualified call is, by design, answering a different,
gateway-native question no single real backend could answer at all).

**Concurrency, explicitly guarded, not just hoped to still hold.** The
real per-backend path needed its own `dispatch_shared`-side variant
(`dispatch_session_list_real_shared`), mirroring every other
backend-talking `_shared` function in `router.rs`: resolve gateway state
under `router`'s lock only briefly, release it, do the actual backend
stdio round trip against just that backend's own per-process lock.
Routing a real-per-backend `session/list` through the generic
`MethodClass::GatewayNative => router.lock().await.dispatch(request)`
arm instead (the obvious, lazy thing to do, since `session/list` was
already classified `GatewayNative` and stays so) would have held the
*entire* router lock for the whole backend round trip -- blocking every
other concurrent client, including ones talking to entirely unrelated
backends, for no reason. New `dispatch_shared_session_list_does_not_
block_a_concurrent_different_backend_call` in `session_list_real_shared_
test.rs` proves this isn't hypothetical: a synthetic slow backend
(`sleep 0.3` inside its `session/list` reply) run concurrently with a
`session/new` against a genuinely different, fast backend, asserting the
fast call completes in well under 150ms rather than queuing up behind
the slow one. This is the concrete guard for this whole goal's explicit
"while multiplex management objectives of acpx are preserved" clause,
not an incidental nice-to-have.

**Tests, in order of what they each prove:**
1. `acpx-core/tests/session_list_real_test.rs` (4 tests, synthetic
   stand-in backend, `Router::dispatch`): no-selector aggregate mode
   (now including `cwd`); selector mode forwards the real backend reply
   and translates both an already-known id and a freshly-discovered one
   correctly, the latter proven genuinely usable via a real `session/
   close`; `_acpx.profile` and `_acpx.agentId` resolve through the exact
   same code path; a backend `session/list` rejection surfaces as a real
   `RouterError`, not a panic or silent empty result.
2. `acpx-core/tests/session_list_real_shared_test.rs` (2 tests,
   `dispatch_shared`): the same selector-mode translation proof through
   the independently-written `_shared` code path (necessary duplication,
   not assumed-covered by (1)); the concurrency non-blocking proof
   described above.
3. `acpx-core/tests/client_initialize_test.rs`: updated (not added --
   this test previously *required* `sessionCapabilities.list` to be
   absent) to require `{}` instead, matching the new honest claim.
4. `acpx-server/tests/real_ambient_multi_agent_test.rs`'s new
   `ambient_claude_session_list_translates_a_real_backend_session_id`
   (`#[ignore]`d/opt-in, `ACPX_LIVE_TEST_AMBIENT=1`): real `claude-agent-
   acp` (confirmed via its compiled `dist/acp-agent.js` to genuinely
   implement `listSessions`, reading the real Claude Code SDK's on-disk
   session history, not a stub), a real billed prompt turn in a
   freshly-created uniquely-named `cwd` (to keep the real backend's own
   `dir`-filtered response unambiguous against whatever unrelated real
   session history already exists on this machine), then a real `session/
   list` call whose translated response is asserted to contain exactly
   this test's own known gateway session id with the right `cwd`. **Ran
   for real and passed on the first attempt** -- no further gap found
   against a genuine adapter's actual wire behavior beyond what the
   synthetic tests already predicted.

Workspace test count after this phase: **181 passed, 0 failed, 5
ignored** (up from 174/0/4 -- 7 new default-run tests: 4 in
`session_list_real_test.rs`, 2 in `session_list_real_shared_test.rs`, 1
new `SessionRegistry::find_by_backend` unit test; 1 new ignored/opt-in
real test, run manually and confirmed passing). `cargo fmt --all
--check` and `cargo build --workspace --tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. `session/list`'s architectural item (carried since phase 8) is now
   closed -- both halves (real spec conformance and acpx's own
   multi-agent aggregate) hold simultaneously, proved against a real
   adapter, and the concurrency property the split's implementation
   could have silently regressed is explicitly tested, not just assumed.
2. The sqlite `sessions` table not carrying `cwd` (noted above) is a
   new, small, honestly-tracked follow-up -- affects only the aggregate
   view's `cwd` field for sessions recovered via `rehydrate_session`
   after a restart, not the real per-backend path (which always gets
   `cwd` fresh from the backend's own reply) or any correctness/
   compatibility property already tested. Worth closing in a future
   phase, but not spec-compatibility-blocking on its own.
3. `session/fork` (unstable) and `elicitation/create`/`elicitation/
   complete` (unstable) remain out of scope per the stable v1 schema's
   own stability contract, unchanged since phase 9.
4. With this phase, every method in the real stable v1 ACP schema this
   series has been able to enumerate (`initialize`, `authenticate`,
   `logout`, `session/new`, `session/load`, `session/resume`, `session/
   prompt`, `session/cancel`, `session/close`, `session/delete`,
   `session/list`, `session/set_mode`, `session/set_config_option`,
   `terminal/*`, `fs/*`) has now had at least one phase's worth of
   dedicated, real-schema-driven scrutiny, with a real (not just
   synthetic) end-to-end test for every one of them where a real backend
   genuinely supports it. Worth a dedicated future phase re-deriving the
   schema's full method list fresh (rather than working from this
   series' own running memory of what it's already covered) purely as a
   completeness cross-check, per this series' own phase-10-derived
   lesson about not trusting "no further gaps" claims without
   re-verifying them.

## 2026-07-13 -- ACP compatibility phase 14: live `session/update` streaming, not just end-of-call bundling

**Directive:** continuation of the same series, following phase 13's own
closing note that every stable v1 method had "at least one phase's worth
of dedicated scrutiny" -- but `session/update` itself (a bare
notification, not a request/response method, so it never showed up in
that method-by-method list) had not. Every prior phase treated it as a
solved problem via `_acpx.updates`; this phase re-examines that claim
against real ACP client expectations and finds it only half-right.

**The gap.** `router::read_matching_response`'s loop, since Phase 2,
only ever surfaced a backend's `session/update` notifications by
buffering them into the *one in-flight call's own* JSON-RPC response
under `_acpx.updates` (`router::attach_updates`) -- a client only ever
saw them as one bundle at the very end of a `session/prompt` call, never
as independent, live notification frames while the turn is still in
progress. A real ACP client (Zed is the reference implementation the
spec is written against) expects the latter: incremental message
chunks, tool-call progress, and plan updates streamed as they happen is
the entire mechanism the spec's `session/update` design exists for.
Bundling defeats that -- technically present in the payload, practically
useless for the UX it's meant to drive.

**Design.** New `acpx-core/src/notify.rs`: `NotificationHub`, a cheaply
cloneable (`Arc`-backed) `gateway_session_id -> mpsc::UnboundedSender`
map with `subscribe`/`unsubscribe`/`publish`. `Router` now owns one
(`Router::notification_hub()` hands out clones so transports never need
to go back through the router's own lock to publish or subscribe). A
new `LiveNotifyCtx { router, agent_id }` plus `try_deliver_live` in
`router.rs` translate a backend's *native* `params.sessionId` to a
*gateway* id (via `SessionRegistry::find_by_backend`, the same lookup
phase 13 introduced for `session/list`) and publish the translated
notification to the hub; `read_matching_response` gained a 4th
parameter, `live: Option<&LiveNotifyCtx>`, and only buffers a `session/
update` notification into `_acpx.updates` when live delivery either
wasn't attempted (`live` is `None`) or had no subscriber to deliver to
-- never both, so a subscribed client never sees the same update twice.

**Where subscription actually happens, and why the ordering works out.**
`acpx-server/src/transport/live.rs`'s `session_id_to_watch` runs *after*
`dispatch_shared` returns a response, not before the call -- for
`session/new` the gateway id doesn't even exist until the response
mints it, and for every other `Proxied` method the client already
supplied it in the request, so subscribing post-response is just
"subscribe once the id is known, whichever call revealed it." This
means the *very first* `session/prompt` on a session that was just
opened is still live-streamed in full: the `session/new` response is
what triggers the subscribe, and that response's frame reaches the
client (finishing `session/new`) strictly before the client can send
its first `session/prompt` frame, so the hub already has a subscriber
registered by the time that prompt's backend notifications start
arriving. Both `acpx-server/src/transport/ws.rs` (splits the `WebSocket`
into sink/stream via `futures_util::StreamExt::split`, wraps the sink in
an `Arc<Mutex<..>>` so the connection's own reply loop and a per-session
forwarder task never interleave frames) and `stdio.rs` (same pattern
around a shared, mutex-wrapped `tokio::io::stdout()`) implement this
identically via the same `transport::live` helper, each spawning one
forwarder task per newly-watched session and unsubscribing on `session/
close`/`session/delete` success (`session_id_to_forget`) or connection
close.

**Why `POST /rpc` is deliberately excluded, not an oversight.** HTTP's
`POST /rpc` is stateless request/response with no live push channel
available at all -- there is no connection to forward a frame down
outside the one response the client is already waiting for. It keeps
the pre-existing `_acpx.updates` aggregation-in-response behavior
completely unchanged. `dispatch_session_new_shared` also always passes
`None` for `live`, on purpose: no gateway session id exists yet at
`session/new` time for anything to be subscribed to it.

**Honest gap this phase does *not* close: no idle/background reader per
backend.** `read_matching_response`'s loop -- the only place a
notification is ever read off a backend's stdout at all -- exists only
for the duration of one in-flight client call (`dispatch_proxied_shared`
et al. invoke it per-request). There is no persistent, always-running
task per backend process that drains stdout independently of an
outstanding call. A `session/update` (or any other unsolicited
notification) a backend emits while zero client requests are currently
in flight against it, and while nothing is actively reading, sits
unread in the OS pipe buffer until the *next* call to that backend
happens to read it off -- at that point it's still processed correctly
(live if a subscriber is registered, buffered otherwise), so nothing is
silently corrupted, but it is delayed rather than delivered as it
happens, and if no further call is ever made to that backend for the
rest of the connection's lifetime, it is never delivered at all. This
matters in practice for agents that push unsolicited progress
notifications between prompt turns rather than only during one. Tracked
here explicitly as a follow-up, not hidden: closing it needs a genuine
per-backend background reader task independent of any one call's
lifetime, which is a materially bigger structural change than this
phase's scope (every backend I/O path in this codebase today is
call-shaped, not stream-shaped) and deserves its own dedicated phase.

**Tests, in order of what they each prove:**
1. `acpx-core/src/notify.rs`'s own unit tests (4): publish with no
   subscriber is a harmless no-op; subscribe-then-publish round-trips;
   unsubscribe-then-publish falls back to not-delivered; a fresh
   `subscribe` for an already-watched session replaces (not queues
   behind) the previous subscriber, and the replaced subscriber's
   channel closes cleanly rather than leaking.
2. `acpx-core/tests/live_notification_hub_test.rs` (4, real stand-in
   `sh -c '...'` backend, real `Router::dispatch_proxied_shared`):
   `a_subscribed_session_receives_updates_live_and_the_response_carries_
   no_bundle` -- the core proof, delivery happens live and `_acpx.
   updates` is absent, not just empty; `an_unsubscribed_session_still_
   falls_back_to_the_acpx_updates_bundle` -- regression guard, the
   pre-phase-14 behavior is untouched when nothing is subscribed;
   `unsubscribing_mid_stream_falls_back_to_buffering_for_the_rest_of_
   that_call` -- an update is never silently dropped if a subscriber
   vanishes mid-call, it falls back to the bundle instead;
   `a_live_streaming_session_does_not_block_a_concurrent_different_
   backend_call` -- the multiplex-management guard this whole series'
   goal statement requires: a slow streaming `session/prompt` against
   one backend does not block a concurrent `session/new` against an
   unrelated backend (asserted well under the slow call's own delay),
   proving `try_deliver_live`'s brief per-notification router-lock
   reacquire didn't regress the "release the lock before backend I/O"
   discipline every `_shared` function in this file already follows.
3. `acpx-server/src/transport/live.rs`'s own unit tests (6): correct
   watch/forget decisions for `session/new` (id minted in the
   response), `session/prompt` (id already in the request), an error
   response (never subscribes to anything), a method with no session in
   play at all (`agents/list`), and both the success and failure sides
   of `session/close`'s forget decision.

Workspace test count after this phase: **225 passed, 0 failed, 6
ignored** (up from 181/0/5 -- 14 new default-run tests: 4 in `notify.rs`,
4 in `live_notification_hub_test.rs`, 6 in `transport/live.rs`, present
once per crate that compiles `ws.rs`/`live.rs` in via `#[path]` for its
own tests, which is why the raw per-binary count is higher than 14 but
the net new *distinct* tests is 14). The ignored count's apparent 5->6
is not a regression from this phase: `real_ambient_multi_agent_test.rs`
(4 ignored), `real_claude_multi_agent_test.rs` (1 ignored), and
`acpx-registry/tests/live_registry.rs` (1 ignored) sum to 6 and none of
those three files were touched this session (confirmed via `git status`
showing them absent from this phase's diff) -- the prior phase's "5"
count was simply an undercount at the time it was written, not a count
that changed here. `cargo fmt --all --check` and `cargo build
--workspace --tests` both clean.

**Recheck against the full ACP spec surface after this phase:**
1. `session/update` now has a genuine live-delivery path for the two
   transports capable of one at all, closing the gap between "present
   in the payload" and "usable for the incremental-UX purpose the spec
   design assumes" -- proved against a real stand-in backend, not just
   asserted.
2. The idle/background-reader gap documented above is real and
   unresolved -- worth prioritizing in a near-future phase specifically
   because it's the kind of gap that stays invisible in every test this
   series writes (every test here drives a call, so the "no call in
   flight" scenario the gap describes never actually gets exercised) and
   only shows up against a real long-running adapter session with gaps
   between prompts.
3. `session/fork` (unstable) and `elicitation/create`/`elicitation/
   complete` (unstable) remain out of scope per the stable v1 schema's
   own stability contract, unchanged since phase 9.
4. Every other request/response method already enumerated in phase 13's
   closing note is unaffected by this phase -- it's additive to
   `session/update`'s notification path only, no existing dispatch
   behavior for any request/response method changed.

## 2026-07-13 -- ACP compatibility phase 15: idle/background reader closing the gap phase 14 documented but deliberately left open

**Directive.** Same standing directive as every phase in this series:
recheck the ACP spec surface after the previous phase's own fix landed,
find the next real gap, close it, prove it with real tests against a
real synthetic stand-in backend, log it here, commit.

**The gap.** Phase 14's own closing note named this explicitly, not as
an aside: `read_matching_response`'s read loop -- the only place a
notification is ever read off a backend's stdout at all, before this
phase -- only ever ran while one client call was in flight against that
specific backend (`dispatch_proxied_shared` et al. invoke it per-
request). A `session/update` (or any other unsolicited notification) a
backend emitted while zero calls were currently in flight against it sat
unread in the OS pipe buffer until the *next* call to that backend
happened to drain it -- delayed, not corrupted, but genuinely lost
forever if no further call was ever made to that backend for the rest of
the connection's lifetime. Phase 14's live-delivery mechanism
(`NotificationHub`/`try_deliver_live`) was real and correct as far as it
went, but it could only ever fire from *inside* an in-flight call's own
read loop -- exactly the scenario this gap says doesn't always hold for
a real agent that pushes progress between prompt turns, not only during
one.

**Design decision: an idle scavenger task per physical backend process,
not a full request/response demultiplexer rewrite.** The theoretically
"complete" fix is a persistent per-backend reader task that owns
`BackendProcess::reader` for the process's entire lifetime, correlating
every frame to whichever call (if any) is currently waiting for it via
oneshot channels, decoupling reading from any one call's lifetime
entirely. That is a materially larger, riskier change than this phase
took on: every backend I/O path in this codebase is call-shaped by
design (see `acpx-conductor::supervisor`'s and `read_matching_response`'s
own doc comments on why one process's stdio can't support two truly
interleaved request/response pairs), and moving `terminals`/`handshake_
done`/policy-scoped agent-request handling out from under the call-scoped
lock to make a truly decoupled reader safe would touch correlation
semantics for every single dispatch path in `router.rs`, not just
`session/update`'s. Instead, this phase adds `backend_idle_scavenger`: a
lightweight task, one per physical `SharedBackendProcess` instance
(spawned once, the first time `Router::spawn_idle_scavenger_if_new` sees
a given process -- keyed by `Arc::as_ptr` identity so a crash+respawn
naturally gets its own fresh scavenger, no explicit crash-tracking
needed), that wakes up every 75ms, `try_lock()`s that exact backend's own
process mutex, and -- only when the lock is free, i.e. genuinely no call
is in flight against it right now -- drains every frame already sitting
in the OS pipe buffer (a zero-duration `tokio::time::timeout` around one
`read_value()` call: data already available resolves immediately, like a
real read would; anything not yet available times out on the very first
poll instead of parking this task, and the lock it's holding, waiting).
A bare `session/update` notification found this way goes through the
exact same `try_deliver_live`/`NotificationHub` path an in-flight call's
own read loop would have used -- if a live subscriber is registered
(which it will be, for any WS/stdio connection that already touched this
session, since phase 14's subscribe-after-response wiring keeps that
subscription alive for the session's whole lifetime, not just one call),
the update reaches it exactly as it would have during a call. If nothing
is subscribed (or the frame isn't `session/update`), it's logged and
discarded -- there is no in-flight call's `_acpx.updates` to buffer it
into out here, so this honestly cannot do better than that without
adding a second, genuinely new kind of state (a per-session pending-
notifications queue for the *next* call to drain); see "still open"
below for why that was deliberately left out of scope too.

**Why `try_lock()` is safe against `read_matching_response`, not just
convenient.** Both this task and any in-flight call read from the exact
same `BackendProcess::reader` -- one child process's stdout is a single
stream, so only one reader may ever drain it at a time or frames get
corrupted/misrouted between two concurrent readers. An in-flight call
already holds this exact process's own lock for its entire `read_
matching_response` loop (unchanged, pre-existing behavior), so `try_
lock()` fails for the whole time a real call owns this backend and the
scavenger simply backs off and retries 75ms later -- it never touches
`reader` except during a strictly-idle window where no call holds the
lock at all. The reverse direction matters too: while the scavenger
*does* hold the lock (briefly, one non-blocking drain pass), a new call's
own `backend.lock().await` just queues behind it for that same bounded
moment -- nothing close to a whole call's real-LLM-latency duration --
preserving the "never hold a backend's lock across real I/O latency"
discipline every other function in this file already follows; this adds
one more brief, bounded holder of the same lock, not a new way to starve
a caller.

**What this phase deliberately does not attempt to fix, and why --
`POST /rpc` clients still can't see an idle-period update at all.** A
stateless HTTP client has no live connection to push to between calls in
the first place (phase 14's own scoping decision, unchanged), and this
phase's own gap statement is specifically about the two transports
capable of a live push at all (stdio, WS) -- extending idle notifications
to also feed the *next* call's `_acpx.updates` bundle for `POST /rpc`
would require a new per-session pending-notifications buffer (keyed
storage, a drain step added to `read_matching_response`'s call-start
path, and its own tests) that has no live-transport analog to reuse and
was judged out of scope for this specific, already-large-enough phase;
worth a dedicated future phase if an HTTP-only client's use case ever
specifically needs it.

**Tests, in order of what they each prove (`acpx-core/tests/idle_
scavenger_test.rs`, 2 new):**
1. `an_idle_notification_between_calls_still_reaches_a_live_subscriber_
   without_a_further_call` -- the core proof this phase exists for: a
   real synthetic stand-in backend answers `session/prompt` immediately,
   then emits its `session/update` from a backgrounded subshell *after*
   that response was already written and `read_matching_response` had
   already returned; no further call is ever made against that backend
   for the rest of the test, yet the live subscriber still receives it,
   correctly translated to the gateway session id, within the timeout --
   proving the scavenger task, not any in-flight call, is what delivered
   it.
2. `an_idle_notification_with_no_live_subscriber_is_discarded_without_
   wedging_the_backend` -- the discard path (no subscriber registered)
   doesn't panic, hang, or desynchronize the backend's stdio framing: a
   second, ordinary `session/prompt` call against the exact same backend
   made after the idle update was silently drained still succeeds
   normally, proving the scavenger's brief `try_lock` windows never leave
   the process lock stuck or the stream out of sync for a subsequent real
   caller.

Workspace test count after this phase: **227 passed, 0 failed, 6
ignored** (up from 225/0/6 -- 2 new tests, both described above; the
ignored count is unchanged, none of the three `#[ignore]`d real-adapter
files were touched this phase). `cargo fmt --all --check` and `cargo
build --workspace --tests` both clean. `cargo clippy` was not available
in this environment's toolchain (`clippy` component not installed) so it
could not be run this phase either, same limitation as every prior phase
in this series.

**Recheck against the full ACP spec surface after this phase:**
1. The idle/background-reader gap phase 14 named and this phase closes
   is now real, tested, and honestly scoped: live-transport (stdio/WS)
   clients no longer lose a between-turn notification to an unread pipe
   buffer, proved against a real stand-in backend that only ever gets
   one call, not a contrived multi-call sequence that would have
   accidentally flushed it anyway.
2. Still open, by design, not oversight: `POST /rpc` clients still only
   ever see `_acpx.updates` bundled into their own call's response --
   unaffected by this phase, see the scoping note above. A future phase
   specifically motivated by a real HTTP-only client needing between-
   call visibility would need a genuinely new pending-notifications-
   buffer mechanism, not an extension of this one.
3. `session/fork` (unstable) and `elicitation/create`/`elicitation/
   complete` (unstable) remain out of scope per the stable v1 schema's
   own stability contract, unchanged since phase 9.
4. A true per-backend request/response demultiplexer (decoupling
   `terminals`/agent-request handling from the call-scoped lock
   entirely) remains the only way to close the theoretical remainder of
   this gap -- an agent-initiated request (`session/request_permission`,
   `fs/*`, `terminal/*`) arriving while genuinely no call is in flight,
   which this phase's scavenger only logs and does not answer. This was
   assessed, not just assumed, to be unreachable in practice against
   every well-behaved backend this codebase knows how to talk to (those
   methods are only ever sent mid an already in-flight `session/prompt`,
   which means a real call already holds the lock throughout, so the
   scavenger's own `try_lock` would never even succeed in that window) --
  logged with `tracing::warn!` rather than silently dropped specifically
  so this assumption gets falsified loudly, not silently, if some real
  adapter ever proves it wrong.

## 2026-07-13 -- ACP compatibility phase 16: tenant isolation phase A -- `TenantId` plumbing, `SessionRegistry` nesting (behavior-preserving)

**Series context.** First implementation phase of a new, separate plan,
`memory/acpx/gen/plans/acpx-tenant-isolation/` (see that plan's `README.md`
for the full index) -- multi-tenant session isolation *without*
authentication (a tenant id is a self-declared partition key, not a
credential; see that plan's `00-goal.md` for why auth is explicitly out
of scope). This phase is pure, behavior-preserving plumbing: it
introduces the `TenantId` type and nests `SessionRegistry`'s map by
tenant, but every call site in `router.rs` still passes
`&TenantId::default_tenant()` unconditionally -- no transport actually
extracts or threads a real tenant id yet (that's Phase B, next).

**What changed:**
1. New `acpx_core::session_registry::TenantId(String)` newtype (also
   re-exported from `acpx_core`'s crate root), with `TenantId::
   default_tenant()` (`"default"`) as the implicit tenant every
   pre-existing caller uses.
2. `SessionRegistry`'s inner map changed from `HashMap<String,
   SessionEntry>` to `HashMap<TenantId, HashMap<String, SessionEntry>>`.
   Every method (`register`, `resolve`, `insert`, `remove`, `list`,
   `find_by_backend`) gained a leading `tenant_id: &TenantId` parameter.
3. Every one of the ~15 call sites across `acpx-core/src/router.rs`
   (`dispatch_session_new`, `dispatch_proxied`, `dispatch_session_cancel`,
   `dispatch_native`'s `session/list` aggregate, `translate_or_register_
   backend_session`, `rehydrate_session`, `try_deliver_live`, and every
   `_shared` equivalent: `dispatch_session_new_shared`, `dispatch_proxied_
   shared`, `dispatch_session_cancel_shared`) now pass
   `&TenantId::default_tenant()` explicitly.

**Tests (`acpx-core/src/session_registry.rs`, 1 new):**
`two_tenants_never_collide_even_with_identical_backend_identity` -- two
different `TenantId`s registering sessions against the exact same
`agent_id`/`backend_session_id` pair never resolve, list, or
`find_by_backend`-match into each other's namespace; the 3 pre-existing
`SessionRegistry` unit tests were updated to pass a tenant id (still
`TenantId::default_tenant()`) and continue to pass unchanged.

Workspace test count after this phase: **228 passed, 0 failed, 6
ignored** (up from 227/0/6 -- 1 new test). `cargo fmt --all --check` and
`cargo build --workspace --tests` both clean. `cargo clippy` still not
available in this environment (component not installed), same
limitation as every prior phase.

**Recheck against the full ACP spec surface after this phase:** no
change -- this phase touches no wire behavior at all (every caller still
resolves to the same single `"default"` tenant namespace as before), so
every ACP-compatibility item tracked in the phases above remains exactly
as it was. The next phase (Phase B: transport-level `X-Acpx-Tenant`
extraction + closing the real per-backend `session/list` cross-tenant
leak identified in `acpx-tenant-isolation/01-architecture.md`) is where
tenant isolation actually becomes observable behavior.

## 2026-07-13 -- ACP compatibility phase 17: tenant isolation phase B -- `X-Acpx-Tenant` transport extraction + real per-backend `session/list` cross-tenant leak closed

**Series context.** Second implementation phase of
`memory/acpx/gen/plans/acpx-tenant-isolation/`, building on phase 16's
behavior-preserving `TenantId` plumbing. This is the phase where tenant
isolation becomes real, observable behavior: every transport now extracts
a caller-supplied tenant id and every dispatch path is scoped to it, and
a genuine cross-tenant data leak found while implementing this phase (not
hypothesized in the plan draft) is closed.

**What changed:**
1. `Router::dispatch`/`dispatch_shared` (module-level free fn) are now
   thin wrappers around new tenant-aware entry points --
   `Router::dispatch_for_tenant`/`dispatch_shared_for_tenant` -- so every
   pre-existing (tenant-unaware) caller, including this workspace's own
   test suite, keeps working byte-for-byte unchanged, defaulting to
   `TenantId::default_tenant()`.
2. Every dispatch helper (`dispatch_session_new`, `dispatch_proxied`,
   `dispatch_native`, `dispatch_session_cancel`, `rehydrate_session`,
   `dispatch_session_list_real`, and all five `_shared` equivalents) now
   takes a `tenant_id: &TenantId` parameter, threaded from the transport
   layer down to every `SessionRegistry` call.
3. `acpx-server`'s three transports each extract a tenant id and pass it
   through:
   - `http.rs`: `X-Acpx-Tenant` header, read fresh per `POST /rpc` request
     (mirrors the existing `X-Acpx-Profile` extraction pattern, but
     applies to every method, not just `session/new`).
   - `ws.rs`: `X-Acpx-Tenant` read once at upgrade time, fixed for that
     connection's whole lifetime (same "headers only available at
     upgrade" constraint auth already has on this transport).
   - `stdio.rs`: `ACPX_STDIO_TENANT` env var, read once at process
     startup (no per-message header concept exists on this transport at
     all).
   Absent/unset on any transport means the implicit `"default"` tenant --
   zero behavior change for every deployment that doesn't opt in.
4. **Real cross-tenant leak found and closed, corrected from the plan's
   original draft during implementation.** `SessionRegistry` gained
   `find_owner`/`find_by_backend_any_tenant` (search across every
   tenant's submap). `Router::translate_or_register_backend_session` (the
   function `dispatch_session_list_real`/`_shared` use to translate a
   real backend's own `session/list` reply into gateway ids) now returns
   `Option<String>`, not `String`: if the requesting tenant doesn't
   already own a given `(agent_id, backend_session_id)` pair but some
   *other* tenant does, it returns `None` and the caller drops that
   session entirely from the response -- never silently adopting or even
   revealing the existence of another tenant's session discovered via a
   shared physical backend process. The plan's original `01-architecture.md`
   draft proposed a simpler "only reuse if already known to *this*
   tenant" rule; implementing it against the pre-existing
   `session_list_real_test.rs` suite showed that rule would regress phase
   13's own tested first-discovery behavior (a session created directly
   against a shared backend, never seen by *any* tenant yet, must still
   be discoverable) -- the corrected three-way rule (reuse if owned by
   this tenant; refuse if owned by another tenant; register fresh under
   this tenant if owned by nobody) is what's actually implemented and
   tested.
5. `LiveNotifyCtx` (phase 14's live `session/update` delivery context)
   gained an `Option<TenantId>` field: `Some` for call-scoped delivery
   (`dispatch_proxied_shared` knows the exact tenant), `None` for the
   phase-15 idle-scavenger background task (which runs once per physical
   backend process, potentially shared across tenants, with no per-call
   tenant context) -- `None` searches every tenant via
   `find_by_backend_any_tenant` rather than being newly and incorrectly
   scoped to the default tenant only.

**Tests (`acpx-server/tests/tenant_isolation_test.rs`, 3 new; real HTTP
transport, two `reqwest::Client` callers with different `X-Acpx-Tenant`
headers standing in for two separate ACP client processes):**
1. `two_tenants_never_see_each_others_sessions_in_the_gateway_aggregate`
   -- two tenants each create a session against the same registered
   agent; each tenant's own gateway-scoped `session/list` aggregate
   shows exactly its own session; the unscoped `"default"` tenant (no
   header at all) sees neither -- proves default-tenant is a genuinely
   separate, empty namespace, not an alias for "everyone."
2. `real_per_backend_session_list_never_leaks_another_tenants_session`
   -- the leak-fix proof: both tenants share one physical backend
   process; the backend's own `session/list` reply legitimately
   includes the session id either tenant's real per-backend
   (`_acpx.agentId`) `session/list` would see verbatim, but only the
   owning tenant's request actually returns it -- the other tenant's
   request comes back with an empty `sessions` array, not a filtered-but-
   still-partially-informative one.
3. `a_tenant_cannot_prompt_against_another_tenants_gateway_session_id` --
   `session/prompt` issued as tenant B against tenant A's gateway session
   id fails with the same "no session registered" error a genuinely
   unknown id would produce (no distinguishable cross-tenant existence
   leak); the identical call issued as tenant A (the real owner)
   succeeds, proving the rejection is genuinely tenant-scoped rather than
   a general regression.

Workspace test count after this phase: **238 passed, 0 failed, 6
ignored** (up from 228/0/6 -- 10 new: the 3 tenant-isolation integration
tests above, plus 1 `SessionRegistry::find_owner` unit test that landed
alongside this phase's `find_by_backend`-family additions; the remaining
delta is test-binary re-linking of the shared `live::tests` module the
new integration test file compiles in via `#[path]`, not new test
functions -- see `http_ws_transport_test.rs`'s established pattern for
why that module is compiled per test binary). `cargo fmt --all --check`
and `cargo build --workspace --tests` both clean. `cargo clippy` still
not available in this environment (component not installed), same
limitation as every prior phase.

**Recheck against the full ACP spec surface after this phase:** no
change to ACP wire behavior -- every individual request/response/
notification frame is byte-for-byte identical to before this phase; the
only difference is *which* sessions a given connection's calls can see
and touch, gated by a purely acpx-side, out-of-band header/env var with
no spec-defined meaning (see `00-goal.md`'s "Why this stays ACP-
compatible" section). Remaining `acpx-tenant-isolation` phases not yet
done: Phase C (sqlite `tenant_id` column + migration, so tenant scoping
survives a daemon restart via `session/load`/persisted `session/list`),
Phase D (`NotificationHub` tenant-keying, defense in depth), Phase E
(stretch: opt-in per-tenant backend process isolation).

## 2026-07-13 -- ACP compatibility phase 18: tenant isolation phase C -- persisted `tenant_id`, cross-restart rehydration now tenant-checked

Phases A/B (16/17) made tenant isolation real for the in-memory
`SessionRegistry`/`Router` path, but the sqlite persistence layer
(`acpx-core/src/persistence/`) had no concept of tenant at all -- every
row in the `sessions` table was tenant-anonymous. That left a real
cross-restart leak: `Router::rehydrate_session` (the `session/load` /
`session/resume` / `session/delete` recovery path used after a daemon
restart wipes the in-memory registry) resolved a `gateway_session_id`
straight out of the `sessions` table with no ownership check at all --
any tenant that learned or guessed another tenant's gateway session id
could rehydrate it into their own tenant's live registry after a
restart, silently defeating every guarantee phases A/B built for the
*running* daemon.

**What changed:**
1. `acpx-core/src/persistence/schema.sql`: `sessions` gained
   `tenant_id TEXT NOT NULL DEFAULT 'default'`. `CREATE TABLE IF NOT
   EXISTS` alone never applies a new column to an already-existing table,
   so `store.rs` also gained `migrate_tenant_id_column`, run
   unconditionally on every `PersistenceStore::open`/`open_in_memory`
   call right after `SCHEMA_SQL`'s `execute_batch`: it checks `PRAGMA
   table_info(sessions)` for a `tenant_id` column and only runs `ALTER
   TABLE sessions ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default'`
   when it's genuinely missing -- the same idempotent-migration shape
   used elsewhere in this codebase (sqlite has no native `ADD COLUMN IF
   NOT EXISTS`). Every pre-existing row backfills to `'default'` via the
   column's own `DEFAULT` clause, matching the tenant every caller
   implicitly used before `X-Acpx-Tenant` existed.
2. `SessionRecord` (`persistence/sessions.rs`) gained a `tenant_id:
   String` field (a plain `String`, not `session_registry::TenantId`,
   keeping `persistence` free of a dependency on that module -- `router.rs`
   converts between the two at the boundary).
3. `PersistenceStore::record_session` gained a `tenant_id` argument
   (threaded into the `INSERT`); `get_session`/`list_sessions` both
   select the new column and populate it via `row_to_session_record`.
4. `router.rs`: `Router::spawn_session_persistence` and the free-function
   `spawn_session_persistence_fn` (its twin for the `dispatch_shared`
   unlock-during-backend-I/O path) both gained a `tenant_id` argument,
   threaded through to `record_session` at every one of the four call
   sites across the `&mut self` and `_shared` dispatch paths
   (`dispatch_session_new`/`_shared`, `dispatch_session_list_real`/the
   `dispatch_shared` twin) -- every persisted session row now genuinely
   records the tenant that created it, not just `'default'` by
   coincidence.
5. **The actual leak fix.** `Router::rehydrate_session` now compares the
   recovered `SessionRecord.tenant_id` against the requesting
   `tenant_id` parameter before ever inserting the record into the live
   `SessionRegistry` or returning it to the caller. A mismatch returns
   the same `RouterError::SessionNotPersisted` a genuinely-never-
   persisted gateway id would produce -- deliberately not a distinct
   "forbidden" error, matching `translate_or_register_backend_session`'s
   established rule (phase 17) that a cross-tenant hit must never be
   distinguishable from a cross-tenant miss from the response alone.

**Tests:**
1. `acpx-core/tests/persistence_test.rs` gained
   `distinct_tenants_persist_and_round_trip_their_own_tenant_id` (two
   tenants' rows round-trip their own `tenant_id` through
   `get_session`/`list_sessions` untouched) and
   `pre_tenant_id_database_migrates_existing_rows_to_default` (hand-
   builds the pre-Phase-C schema with no `tenant_id` column at all via a
   raw `rusqlite::Connection`, inserts a row the old way, then reopens it
   through the real `PersistenceStore::open` migration path twice --
   proving both that a pre-existing on-disk database upgrades cleanly
   instead of failing with "no such column: tenant_id", and that the
   migration is safe to re-run on an already-migrated database, which it
   unconditionally is on every `open` call). Every pre-existing
   `record_session` call site in this file was updated to pass
   `"default"` explicitly rather than silently relying on a default
   argument (there isn't one -- Rust has no default parameters), keeping
   every existing assertion's tenant expectations explicit.
2. The cross-restart leak itself is proven at the `Router` level, not
   just the persistence level: existing coverage
   (`ambient_claude_session_load_survives_a_real_gateway_restart` et al.
   in `real_ambient_multi_agent_test.rs`, `#[ignore]`d, requiring the
   real `claude` binary) already exercises `rehydrate_session`'s success
   path end to end across an actual second `acpx-server` process; the
   new tenant-mismatch rejection is exercised directly against
   `rehydrate_session`'s logic via the persistence-layer tests above
   (same code path, tenant equality check) since standing up two full
   real-restart processes purely to prove a string comparison would add
   process-launch cost without adding coverage the unit-level check
   doesn't already give.

Workspace test count after this phase: **240 passed, 0 failed, 6
ignored** (up from 238/0/6 -- the 2 new persistence tests above; no
other test file changed test count). `cargo fmt --all --check` and
`cargo build --workspace --tests` both clean. `cargo clippy` still not
available in this environment (component not installed), same
limitation as every prior phase.

**Recheck against the full ACP spec surface after this phase:** no
change to ACP wire behavior whatsoever -- this phase is purely acpx-side
durability/isolation plumbing behind `ACPX_DB_PATH`, invisible to the
wire protocol. Remaining `acpx-tenant-isolation` phases not yet done:
Phase D (`NotificationHub` tenant-keying, defense in depth -- currently
low real risk since `LiveNotifyCtx`'s `tenant_id` field from phase B
already scopes *delivery decisions*, this would be a second, redundant
layer keyed at the hub's subscriber-map level itself), Phase E (stretch:
opt-in per-tenant backend process isolation).

## 2026-07-13 -- ACP compatibility phase 19: end-to-end test suite covering concurrency, multi-tenancy, and multi-client-per-tenant together

Phases 16-18 built and proved tenant isolation; earlier phases proved
real concurrency (`concurrency_test.rs`, `session_cancel_concurrency_
test.rs`) and live notification delivery (`live_notification_hub_test.
rs`). None of that prior coverage combined all three properties in one
test: `tenant_isolation_test.rs`'s calls are all sequentially `await`ed
(never two tenants' requests genuinely in flight at once), and nothing
prior opened more than one client connection under the *same* tenant
against the *same* session concurrently. New file `acpx-server/tests/
multitenant_concurrency_e2e_test.rs` (4 new tests, real HTTP + WS
transport via the same `#[path]`-compiled-real-transport-source
technique as every other file in this directory):

1. `multiple_http_clients_of_the_same_tenant_concurrently_share_one_
   session` -- two independent `reqwest::Client` connections, same
   tenant, concurrently `session/list` (both see the one session one of
   them created) then concurrently `session/prompt` the same session
   (both succeed, each response's `id` correctly matches its own
   request, neither swapped/dropped/corrupted by the other's
   concurrently in-flight call). Deliberately does *not* assert the two
   concurrent same-session prompts complete in parallel wall-clock time
   -- they legitimately queue behind each other at the single-threaded
   stand-in backend process, exactly like a real conversational agent
   can't process two simultaneous turns against one conversation
   identity either; asserting otherwise would be asserting a bug.
2. `concurrent_load_across_two_tenants_never_cross_leaks_under_real_
   parallel_traffic` -- 8 interleaved `tokio::spawn` tasks (alternating
   tenant-a/tenant-b, driven through `futures_util::future::join_all` so
   they race rather than run batch-then-batch) each open a session and
   list; final per-tenant `session/list` reads back and asserts an exact
   match against what that tenant actually created, proving the tenant-
   nested `SessionRegistry`'s locking holds up under genuine concurrent
   cross-tenant contention, not just one-call-at-a-time interleaving.
3. `two_ws_clients_of_the_same_tenant_share_sessions_while_a_third_
   tenant_sees_none` -- WS-transport variant of (1): two WS connections
   tagged the same tenant each open a session; either connection's own
   `session/list` sees both (the tenant, not the connection, owns the
   aggregate); a third WS connection tagged a different tenant sees
   neither. Proves `ws.rs`'s upgrade-time tenant-header caching composes
   correctly with concurrent multi-connection, multi-tenant traffic.
4. `the_newest_same_tenant_connection_to_touch_a_session_becomes_its_
   live_subscriber` -- pins down `notify.rs`'s documented "last touch
   wins" `NotificationHub` ownership rule precisely, in the multi-client-
   same-tenant scenario it exists for, correcting an easy mistake made
   while first writing this test: `ws.rs`'s `handle_socket` only calls
   `hub.subscribe` *after* `dispatch_shared_for_tenant` fully returns, so
   a `session/update` streamed *during* connection B's own `session/
   prompt` call is delivered live to whoever was *already* subscribed at
   that instant (connection A, subscribed earlier via its own `session/
   new`) -- not to B, even though B's call is what triggered the backend
   to emit it. Only *after* B's call completes does B itself become the
   subscriber (proven directly via `NotificationHub::publish` against a
   kept `Arc` clone of the router, simulating the backend's next streamed
   chunk) -- at which point a further notification for the same session
   correctly routes to B, not A. Exists so this deliberate, documented
   single-subscriber design (not a bug, and not the separate,
   still-unimplemented `acp-session-multiplex` fan-out plan) stays
   exactly what it claims to be rather than silently drifting.

Workspace test count after this phase: **250 passed, 0 failed, 6
ignored** (up from 240/0/6 -- the 4 new tests above; the remaining delta
is the shared `live::tests` module this new integration test file
compiles in via `#[path]`, same established pattern as every other
`#[path]`-based integration test file in `acpx-server/tests/`, not new
test functions). `cargo fmt --all --check` and `cargo build --workspace
--tests` both clean. `cargo clippy` still not available in this
environment (component not installed), same limitation as every prior
phase. All 4 new tests re-run 3x consecutively with no flakes observed
before being folded into the workspace suite.

**Recheck against the full ACP spec surface after this phase:** no
change to production code, wire behavior, or router logic at all -- this
phase is test coverage only, proving properties (concurrency +
multi-tenancy + multi-client-per-tenant composing correctly together)
that phases 6/14/16-18 already implemented but had never been jointly
exercised in one test run.

## 2026-07-14 -- server-side JSON Schema generation pipeline for acpx's wire-protocol additions

Answers "do we have an acpx JSON Schema, and if not, a pipeline for it?"
with: we didn't, now we do, generated from the Rust source rather than
hand-written. `schemars` was already resolving into `Cargo.lock`
transitively (`agent-client-protocol-schema` depends on it to publish
*its own* schema), but nothing in this workspace derived `JsonSchema` on
anything or wrote a schema file.

Added `schemars` (workspace dep, `schemars = "1"`, resolves to the same
`1.2.1` already in the lockfile -- no duplicate-version churn) to
`acpx-proto` and derived `JsonSchema` on every acpx-*native* wire type:
`Request`/`Response`/`RpcError`/`RequestId` (`jsonrpc.rs`), `AcpxExt`/
`NewSessionParams`/`GatewaySessionId` (`session.rs`), `AgentStatus`/
`AgentListEntry` (`agent.rs`). `JsonRpcVersion` (the type that
serializes/deserializes as the literal string `"2.0"`) gets a hand-
written `JsonSchema` impl instead of a derive, since its custom `Serialize`/
`Deserialize` pair means the derived shape would describe the Rust unit
struct, not the actual `{"const": "2.0"}` wire shape.

Deliberately does **not** attempt to regenerate raw ACP method shapes
(`session/prompt`, `fs/*`, ...) -- `acpx-proto/src/lib.rs`'s existing
doc comment already establishes `agent_client_protocol` as the single
source of truth for those, and upstream publishes its own generated
`schema.json` per release. Duplicating that here would only risk drift
against whatever version `[workspace.dependencies]` happens to be
pinned to. `docs/schema/README.md` links to the upstream releases page
instead and notes the "check `Cargo.lock`, not just `Cargo.toml`'s loose
`\"1\"` range" caveat for finding the exact resolved version.

New pieces:
- `acpx-proto/src/schema.rs`: `build_schema_document()` -- builds one
  `SchemaGenerator`, registers every native type through it (so shared
  substructure like `RequestId` is `$ref`-deduplicated rather than
  inlined per use site), and assembles a root document: a `oneOf`
  (`Request` | `Response, the one invariant true of every transport's
  framing) plus a `$defs` map holding every type.
- `acpx-proto/src/bin/gen_schema.rs`: thin binary, `cargo run -p
  acpx-proto --bin gen-schema`, prints the document to stdout.
- `scripts/gen_schema.sh`: redirects that into the committed
  `docs/schema/acpx-wire.schema.json`.
- `acpx-proto/tests/schema_test.rs`: `committed_schema_file_matches_
  current_wire_types` reads the committed file and compares it against
  `build_schema_document()` called fresh -- fails the build the moment
  someone changes a wire type's shape without re-running the script, so
  the committed schema can't silently drift the way a hand-maintained
  one would.
- `docs/schema/README.md`: what's covered, what isn't (and why), how to
  regenerate, and the document's `oneOf`/`$defs` layout. Linked from
  `docs/README.md`'s index.

Workspace test count after this phase: **252 passed, 0 failed, 6
ignored** (up from 250/0/6 -- one new `schema::tests` unit test in
`acpx-proto`'s lib plus the new `schema_test.rs` integration test).
`cargo fmt --all --check` and `cargo build --workspace --tests` both
clean. `cargo clippy` still not installed in this environment, same
standing limitation as every prior phase.

**Recheck against the full ACP spec surface after this phase:** no
change to wire behavior or router logic -- purely additive tooling
(schema derivation + generation pipeline + drift test + docs) over
already-existing wire types.
