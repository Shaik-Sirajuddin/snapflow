# ACPX session-stream panel review

Reviewed 2026-08-03 after protocol-fix workstreams.

Resolved:

- `thread_actor.rs` now maintains a per-session queue projection, applies
  inserted/sent_prompt/removed deltas, and replaces it on snapshots.
- Server-owned queue send-now uses the queue mutation effect; local queues retain
  immediate prompt behavior.
- Permission-profile list/get/set calls are exposed through the gateway actor and
  `AgentBridge` wrappers.
- `acpx/agent_resolution` is parsed and losing approval requests are removed from
  the panel's pending-request state.

Verification:

- `cargo check --manifest-path panel-rust/Cargo.toml` passes.
- Queue reducer tests and queue-delta/parser tests pass.
- The production-scoped protocol/UI gates are green; the monolithic `--lib`
  harness still contains host-input/snapshotd-dependent keyboard and lifecycle
  probes that are intentionally outside this protocol change. `cargo test
  --tests --no-run` compiles all integration targets.

The serial `slint_component_e2e_test` probe is green: 21/21 passed. The
fixtures now model the gateway-ready state, and the UI exposes the current
Harness override rows, terminal popup actions, MCP fields, and global-skills
toggle wiring.
