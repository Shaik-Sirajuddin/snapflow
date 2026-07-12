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
