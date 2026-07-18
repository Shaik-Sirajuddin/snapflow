# acpx native setup

A quick-start guide for running `acpx-server` directly as a single ACP
gateway daemon -- no ACP-compatibility bridge, no strict fixed model
list. This is acpx's own native surface: profiles, providers, MCP
servers, session retention administration, and every gateway-native
method (`agents/*`, `profiles/*`, `mcp_servers/*`,
`session/retention/*`), on top of full raw ACP passthrough
(`session/new`, `session/prompt`, `session/load`, ...).

For the strict-ACP-client-compatible surface instead (Zed, OpenHands
with model discovery), see
[`README.acp-compatibility.md`](./README.acp-compatibility.md).

For the full reference (every environment variable, every method, the
provisioning file schema, architecture), see [`docs/`](./docs/) --
this file is deliberately a short path to a first working daemon, not a
replacement for it.

## 1. Prerequisites

- Rust stable (`rustc`/`cargo`) if building from source, or use the
  release binary attached to this repository's GitHub release.
- Whatever runtime your chosen backend agent needs: `node`+`npm` for an
  `npx`-distributed adapter (`codex-acp`, `claude-agent-acp`), `uv` for
  a `uvx`-distributed one.
- Real Claude/Codex/other-provider credentials, however you'd normally
  authenticate that CLI/adapter (ambient CLI login, or an API key via a
  profile -- see step 4).

## 2. Build (or use the release binary)

```sh
git clone https://github.com/Shaik-Sirajuddin/multi_media_main.git
cd multi_media_main/acpx
cargo build --release -p acpx-server
# binary: target/release/acpx-server
```

## 3. Minimal run -- native/unmanaged mode

No config file needed. This example uses `codex-acp` against an
already-`codex login`'d machine (ambient auth, no API key required):

```sh
ACPX_BACKEND_CMD="npx -y @agentclientprotocol/codex-acp@1.1.2" \
ACPX_HTTP_BIND="127.0.0.1:8790" \
  target/release/acpx-server
```

Any HTTP/JSON-RPC client can now talk to `POST http://127.0.0.1:8790/rpc`,
or upgrade `GET /ws` for a persistent connection with live
`session/update` streaming. `acpx-server` also serves its own stdin/stdout
as a stdio ACP endpoint concurrently -- point a stdio-based ACP client
(Zed's generic "custom agent" launcher, for example) directly at the
binary + the same env vars, no HTTP involved at all.

## 4. Managed mode -- profiles, providers, real API keys

For more than one backend/provider, provision profiles at startup via
`ACPX_CONFIG_FILE` (a JSON file; `secret_env` reads the actual key from
an env var rather than the file itself):

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

```sh
ACPX_CONFIG_FILE=/path/to/acpx-config.json \
ANTHROPIC_API_KEY="sk-..." \
ACPX_HTTP_BIND="127.0.0.1:8790" \
  target/release/acpx-server
```

A client selects `work-claude` via `session/new`'s `params._acpx.profile`.
Omitting `_acpx.profile` entirely stays in native/unmanaged mode against
`ACPX_DEFAULT_AGENT_ID`, regardless of what's provisioned.

## 5. Durable sessions and retention (optional)

Set `ACPX_DB_PATH=/path/to/sessions.sqlite3` to persist session
metadata/transcripts across restarts, and add
`ACPX_STARTUP_SESSION_RECOVERY_ENABLED=1` to restore load/resume-capable
sessions automatically before either transport starts accepting
requests. Idle sessions are safely closed by a background reaper
(`ACPX_SESSION_IDLE_TTL_SECONDS`, default 30 minutes) that never
interrupts an in-flight turn; pin a session against that via the
gateway-native `session/retention/pin` method.

See [`docs/setup.md`](./docs/setup.md) for the complete environment
variable reference (lifecycle/retention, multi-tenant, process
isolation, admin surface) and [`docs/architecture.md`](./docs/architecture.md)
for how it all fits together.

## 6. Verify

```sh
cd acpx
./scripts/self_test.sh
```

Boots a real `acpx-server` against a trivial synthetic backend and runs
the bundled `acpx-selftest` CLI against it over real HTTP -- confirms
the binary/build actually works without touching any real backend or
API key.
