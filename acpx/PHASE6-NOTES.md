# Phase 6 notes (`04-phased-plan.md` steps 23-24)

## Step 23: does `agent-client-protocol-test` exist, and is it usable?

**Short answer: it exists as source in the official SDK's repo, but it is
explicitly `publish = false` -- it has never been, and is not intended to
ever be, published to crates.io. Not adopted.**

`05-open-risks.md` flagged this crate's actual contents as unverified. Here
is what a direct check found:

- **crates.io**: `curl https://crates.io/api/v1/crates?q=agent-client-protocol-test`
  returns zero results. There is no published crate by this name, at any
  version, ever.
- **docs.rs**: `https://docs.rs/agent-client-protocol-test` returns HTTP 404 --
  docs.rs only builds docs for crates that exist on crates.io, so this is
  consistent with the above.
- **GitHub source**: the crate *does* exist as a workspace member of
  [`agentclientprotocol/rust-sdk`](https://github.com/agentclientprotocol/rust-sdk),
  at `src/agent-client-protocol-test/`. Its own `Cargo.toml` says why it's
  unpublishable:
  ```toml
  [package]
  name = "agent-client-protocol-test"
  version = "0.11.0"
  description = "Test utilities and mock implementations for the Agent Client Protocol"
  publish = false
  ```
  `publish = false` is a deliberate, permanent opt-out, not an oversight
  waiting for a future release -- this is a workspace-internal dev
  dependency for the rust-sdk repo's own doctests/examples, not a
  distributable library.

### What it actually provides

Reading `src/agent-client-protocol-test/src/lib.rs` and its sibling
modules (`arrow_proxy.rs`, `test_binaries.rs`, `testy.rs`):

- A `MockTransport` that **panics if actually invoked** -- its own doc
  comment says it exists only so the SDK's doctests have something to name
  as a type parameter, never to run.
- A pile of ad hoc request/response/notification structs (`MyRequest`,
  `ProcessRequest`, `AnalyzeRequest`, `ValidateResponse`, ...) with
  hand-written `JsonRpcMessage` impls via a local macro -- these are
  scenario-specific fixtures for the SDK's own `arrow_proxy`/`testy`
  examples (proxy-chaining demos), not general ACP spec fixtures or
  request/response pairs for the real `session/new`/`session/prompt`/etc.
  methods `acpx-proto` actually implements.
- Two `[[bin]]` targets (`mcp-echo-server`, `testy`) -- standalone example
  binaries, not a library surface `acpx-core`'s tests could import.
- No generic mock `Agent`/`Client` implementation of the real ACP
  interface that a proxy/router (like `acpx-core::router::Router`) could
  point at as a fake backend. The closest thing, `arrow_proxy`, is
  specifically a proxy-chain demo for `agent-client-protocol-conductor`
  (single client/agent pair proxying), not a multi-agent gateway harness.

### Would it be worth adopting even if it were publishable?

No, for two independent reasons:

1. **It cannot be a normal dependency.** `publish = false` means the only
   way to depend on it at all is a `git`/`path` dependency pinned to a
   specific commit of `agentclientprotocol/rust-sdk`, tracking an
   unversioned, no-semver-guarantee internal crate. That's a much worse
   maintenance posture than the `agent-client-protocol` crate `acpx-proto`
   already depends on for real (which *is* published, versioned, and
   pinned as a workspace dependency per Phase 0 step 1).
2. **Its actual content doesn't fit acpx's architecture even ignoring
   publishability.** Everything in it (`MockTransport`, the scenario
   fixtures, `arrow_proxy`) is oriented around the SDK's own doctests and
   a single-client/single-agent proxy-chain demo. `acpx-core::router::Router`
   is a remote-gateway, N-backend, session-registry-keyed router, not a
   1:1 stdio proxy -- there's no `Role`/`ConnectTo` trait object in this
   crate's design for a `MockTransport` to stand in for in the first place.
   The workspace's actual pattern for "fake backend" (a real `sh -c '...'`
   subprocess that speaks newline-delimited JSON-RPC over stdio, per
   `router_dispatch_test.rs`'s doc comment) already gives every test in
   this workspace a *real* process with *real* framing/spawn/stdio
   behavior, which is strictly more representative of a real
   `codex-acp`/`claude-agent-acp` backend than an in-process mock struct
   would be -- adopting `agent-client-protocol-test` would mean building a
   second, less-realistic fake-backend mechanism alongside the one that
   already works and is used everywhere.

`acpx-proto`'s own round-trip tests (`jsonrpc::tests::request_round_trips`,
`session::tests::*`, see `COVERAGE.md`'s Phase 1 row) already cover "do
`acpx-proto`'s types round-trip correctly" using the real
`agent-client-protocol` types directly (`serde_json` round trips), which
was the other half of what step 23 asked this crate to help with -- that
need is already met without this dependency.

**Conclusion: not added to `acpx-core/Cargo.toml`.** No dependency change
was made for this step. `05-open-risks.md`'s open item is now closed
(verified, not adopted) rather than left unverified.

## Step 24: acpx's own gateway-native test suite

Added `acpx-core/tests/gateway_native_coverage_test.rs` (8 tests), covering
the specific gaps `COVERAGE.md`'s "Gaps" section called out as not yet
exercised via full `Router::dispatch` (as opposed to a store's own unit
tests, which already existed for most of this surface):

- **Node/npm-missing status, via `agents/status` and `agents/list`**
  (`agents_status_reports_runtime_missing_when_node_and_npm_absent_from_path`,
  `agents_list_reflects_runtime_missing_for_every_npx_only_agent_when_path_is_empty`).
  `acpx-core/tests/agents_gateway_native_test.rs` already asserted the
  *positive* case (`status == "installed"`, since this environment has a
  real `node`/`npm`) but nothing exercised the `runtime_missing` branch of
  `detect::detect` through a full dispatch. These two tests temporarily
  rewrite the process's `PATH` env var to empty so `detect`'s `which()`
  checks genuinely fail, then restore it immediately after, before any
  assertion runs (so a panicking assertion can't leave `PATH` broken for
  a later test).
- **`agents/install` error paths**
  (`agents_install_with_unknown_agent_id_errors`,
  `agents_install_with_missing_id_param_errors`). Previously only the
  success path was exercised via `Router::dispatch`
  (`agents_gateway_native_test.rs`'s
  `agents_install_for_npx_agent_succeeds_when_node_npm_present`); both of
  `agents/install`'s distinct `RouterError` variants
  (`MissingAgentId`/`UnknownAgentId`) are now covered.
- **`profiles/update` on a name that was never created**
  (`profiles_update_on_missing_name_errors_via_dispatch`). Duplicate-create
  and delete-missing were already covered by
  `profile_resolution_test.rs`'s `profiles_crud_round_trips_via_dispatch`;
  update-on-missing was the one CRUD error path that test didn't reach.
  Also added `profiles_delete_on_missing_name_errors_twice_in_a_row` as a
  standalone delete-on-a-name-that-never-existed case (the existing
  coverage only deleted a name that *had* existed a moment before).
- **`session/list` edge cases**
  (`session_list_on_a_fresh_router_is_empty_not_an_error`,
  `session_list_aggregates_across_multiple_distinct_agents`). The existing
  `router_dispatch_test.rs::session_list_aggregates_registered_sessions`
  only covered a single agent with one session; the empty-registry case
  (no sessions registered at all) and true multi-agent aggregation (two
  distinct `agentId` values in one `session/list` response) were both
  untested. Native mode always routes through one fixed
  `default_agent_id`, so multi-agent aggregation is exercised here via two
  profiles (`resolve_profile` registers each profile's sessions under a
  `profile:<name>` supervisor key distinct from the agent id it wraps),
  the only full-dispatch path that produces more than one `agentId` today.

All eight tests use the same synthetic `sh -c '...'` stand-in-backend
pattern as `router_dispatch_test.rs` wherever a fake backend process is
needed, per this task's scope.

### A note on the `PATH`-mutation tests

Two tests in the new file temporarily set `PATH=""` for the whole test
process to simulate a missing `node`/`npm` runtime, since `detect.rs`'s
`which()` check shells out via `std::process::Command`, which resolves
the binary against the *current* process environment at spawn time --
there's no dependency-injection seam for a fake `PATH` in `detect.rs`
itself (and adding one was out of this task's scope, which only permitted
adding a new test file, not touching `acpx-core/src/`).

Because Rust's default test harness runs every `#[test]`/`#[tokio::test]`
function in one file concurrently on separate OS threads within the same
process, and `PATH` is process-wide state, every test in this new file
(not just the two that mutate `PATH`) acquires a shared `std::sync::Mutex`
lock (`serialize()`) before running its body -- otherwise a `PATH=""`
window in one test could race against another test's `sh` stand-in-backend
spawn (which also needs to resolve `sh` via `PATH`) and produce a flaky,
environment-order-dependent failure. This only serializes tests *within
this one file* -- integration test files are separate binaries/processes
in Cargo, so no other test file in the workspace is affected. Verified
stable across three consecutive full `cargo test` runs of this file at the
default (parallel) thread count.
