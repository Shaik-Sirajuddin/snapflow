# acpx test coverage matrix

This document tracks, phase by phase, what's implemented in the `acpx`
workspace and what actually has test coverage -- as opposed to what the
plan merely describes. Step numbers below match
`memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md`; update this
file as each subsequent phase lands rather than letting it drift out of
sync with the code. Every row reflects a real `cargo test --workspace`
run and an actual read of the referenced test file(s) -- not an
aspirational claim of what should exist.

As of this update: `cargo test --workspace` passes **101 tests, 0
failures, 1 explicitly `#[ignore]`d** (the live-registry network test, see
Phase 4 below). `cargo build --workspace` and `cargo fmt --all --check`
are both clean.

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

**Not yet built in Phase 3:** a config-file/CLI/env surface for actually
provisioning `ProviderConfig`s and secrets into a running `acpx-server` --
`Router::register_provider`/`Router::store_key` exist as a programmatic
seam (used by the tests above) but `acpx-server`'s `main.rs` doesn't call
them yet, so a real deployment currently has no way to configure a
provider/profile without writing Rust. Tracked as a followup, not silently
missing.

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

## Gaps / not yet covered

Pulled from `memory/acpx/gen/plans/acp-gateway-daemon/05-open-risks.md` --
these are acknowledged, not newly discovered:

- **`ext::registry::install`'s progress/job model is still undecided** (Phase 5 step 22) -- the client can now trigger installation for real, but a slow `npx`/`binary` install has no way to report incremental progress back to a waiting caller; `05-open-risks.md`'s "client-initiated installer needs a progress/job model" item is directly relevant now that this call path exists, not just a future concern.
- **No live-registry test runs by default.** `acpx-registry/tests/live_registry.rs`'s `live_registry_matches_expected_shape` is `#[ignore]`d (hits the real network); only `registry.fallback.json` parsing is covered in the default test run.
- **No real `npx`-installed-agent end-to-end test.** Every test in this workspace uses a synthetic `sh -c '...'` stand-in backend (see `router_dispatch_test.rs`'s doc comment for the pattern) rather than a real `codex-acp`/`claude-agent-acp`/gemini adapter -- Phase 6 step 26 (detection -> installation -> use, for real, per agent) is not started.
- **No Windows/macOS test coverage for the `binary` distribution's download+extract path** -- `install.rs`'s zip/tar.gz sniffing is unit-tested, but only exercised on Linux in this environment; `05-open-risks.md` explicitly calls out that this path needs testing on all three OSes before being considered done.
- **No encryption at rest for the keystore.** `keystore.rs` is explicit in its own doc comment: secrets live in-memory only, process restart forgets them, and no encryption-at-rest mechanism has been chosen yet (`05-open-risks.md`'s "Key storage mechanism is unspecified" item is still open).
- **`claude-agent-acp`'s `ANTHROPIC_BASE_URL` support is researched, not verified against a real running adapter** -- see Phase 3 step 16's row above and `05-open-risks.md`.
- **One process per profile, not one process per session.** Re-resolving an already-running profile (e.g. after a `profiles/update` changes its provider/key) does not restart its already-running supervised process -- documented as a known gap in `router.rs`'s `resolve_profile` doc comment, tracks `05-open-risks.md`'s "one process per backend vs. one process per session" item.
- **No transport security (auth/TLS) for the HTTP/WS remote-access transport** -- binds to `127.0.0.1:8790` by default; `05-open-risks.md`'s "Transport security for remote access" item is unresolved.
- **No reverse-direction (agent-initiated) message routing** -- `session/update` notifications, `session/request_permission`, etc. arriving on a backend's stdout without a matching request id are logged and dropped (`router.rs`'s `read_matching_response` doc comment), not routed back to the owning client connection; `05-open-risks.md` flags this as unresolved.
- **No provider/profile provisioning surface in `acpx-server` yet** -- see Phase 3's "Not yet built" note above.
- Phase 6 (end-to-end test suite spanning all phases -- the official `agent-client-protocol-test` crate's actual API is still unverified per `05-open-risks.md`, and the reusable Zed-style `common_e2e_tests!`-pattern harness across Claude/Codex/Gemini has not been started) is the only remaining unstarted phase.
