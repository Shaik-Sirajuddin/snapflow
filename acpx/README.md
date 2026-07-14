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

All six phases in `04-phased-plan.md` are implemented (workspace skeleton
through the end-to-end test suite), plus a post-Phase-6 black-box
self-test layer, a real multi-agent concurrency fix, a real
`claude-agent-acp` adapter e2e test, and a self-review pass (concurrency/
multi-client/auth/memory). See `COVERAGE.md` for the full phase-by-phase
implementation/test-coverage matrix and the honestly-tracked residual
gaps (encryption at rest for the keystore, TLS, install progress/job
model, etc.) -- nothing below is aspirational, every row there reflects a
real `cargo test --workspace` run.

## Configuration

`acpx-server` is configured entirely via environment variables (no config
file is required for a minimal single-agent deployment):

- `ACPX_BACKEND_CMD` -- space-separated program + args for the default/
  native-mode backend (default: `npx -y @agentclientprotocol/codex-acp@1.1.2`).
- `ACPX_DEFAULT_AGENT_ID` -- id that command is registered under (default: `default`).
- `ACPX_HTTP_BIND` -- HTTP/WS bind address (default: `127.0.0.1:8790`, loopback only).
- `ACPX_AUTH_TOKEN` -- if set, requires `Authorization: Bearer <token>` on `POST /rpc` and the `GET /ws` upgrade; unset means no auth (still no TLS -- pair with a TLS-terminating reverse proxy for any non-loopback bind).
- `ACPX_DB_PATH` -- sqlite file for session metadata + transcripts; unset skips persistence entirely.
- `ACPX_CONFIG_FILE` -- path to a JSON file declaring providers/central MCP servers/profiles to provision at startup, before either transport starts accepting requests. See `acpx-server/src/provisioning.rs`'s doc comment for the full schema and the `secret`/`secret_env` distinction (`secret_env` -- reading the actual value from an env var rather than the file -- is the recommended shape for anything beyond local testing). A malformed or rejected file fails startup outright rather than booting a partially-configured gateway. Example:

  ```json
  {
    "providers": [
      {"name": "anthropic-default", "kind": "anthropic", "base_url": null}
    ],
    "mcp_servers": [
      {"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]}
    ],
    "profiles": [
      {
        "name": "work-claude",
        "agent_id": "claude-agent-acp",
        "provider": "anthropic-default",
        "secret_env": "ANTHROPIC_API_KEY",
        "mcp_servers": ["fs"]
      }
    ]
  }
  ```

  Before this, `Router::register_provider`/`Router::store_key` were
  programmatic-only seams exercised solely by this workspace's own tests
  -- a real deployment had no way to provision a provider/profile without
  writing Rust. This closes that gap.

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
