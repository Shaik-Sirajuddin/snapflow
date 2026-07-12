# acpx

A Rust gateway daemon that sits in front of multiple ACP (Agent Client
Protocol) backend agents (Claude, Codex, Gemini, ...) and presents one
consistent ACP surface to clients, regardless of which backend agent
actually services a session.

See the design docs at
`../memory/acpx/gen/plans/acp-gateway-daemon/README.md` for the full plan
(goal, architecture, crate layout, phased plan, open risks). This workspace
is being built out phase by phase per that plan; status is tracked below.

## Workspace layout

- `acpx-proto` -- shared ACP wire types (JSON-RPC envelope + `session/*`/`agent/*` payloads).
- `acpx-registry` -- remote adapter registry client (official ACP registry).
- `acpx-conductor` -- backend process supervision (spawn/stop/restart, stdio JSON-RPC framing).
- `acpx-core` -- the gateway's brain: session registry, profiles, providers, router, persistence.
- `acpx-server` (bin) -- daemon entrypoint: stdio/HTTP/WS transports + gateway API.
- `acpx-client` -- Rust client SDK for consumers of the gateway.

## Status

Phase 0 (workspace skeleton) in progress. See the plan's
`04-phased-plan.md` for the full phase breakdown.

## Self-test

`cargo test --workspace` is the primary correctness test suite. On top of
that, `scripts/self_test.sh` is a one-shot, black-box smoke test that ties
the whole workspace's self-test story together for a human or CI to run: it
builds the workspace, boots a real `acpx-server` against a trivial stand-in
backend, and runs the `acpx-selftest` CLI (`acpx-server/src/bin/selftest.rs`)
against it end-to-end over HTTP, the way an external client would.

Run it from the `acpx/` directory:

```sh
./scripts/self_test.sh
```

It prints a final `PASS`/`FAIL` line and exits with `acpx-selftest`'s own
exit code.
