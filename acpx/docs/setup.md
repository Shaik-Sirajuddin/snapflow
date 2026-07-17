# Setup

## Prerequisites

- Rust stable toolchain (`rustc`/`cargo`; this workspace is built/tested
  against `1.97`, no `rust-toolchain.toml` pin -- any reasonably recent
  stable should work). Install via [rustup](https://rustup.rs) if not
  already present.
- Whatever runtime your chosen backend agent needs: `node`+`npm` for an
  `npx`-distributed adapter (e.g. `codex-acp`, `claude-agent-acp`), `uv`
  for a `uvx`-distributed one. `acpx-core`'s `detect.rs` checks for these
  on `PATH` and reports `RuntimeMissing`/`NotInstalled` via
  `agents/list`/`agents/status` if absent.
- If you're using managed profiles against a real provider (Anthropic/
  OpenAI/litellm), the relevant API key. If you're using native mode
  against an already-`claude`/`codex`-CLI-logged-in machine (ambient
  auth), no key is needed -- the adapter reuses that CLI's own login.

## Build

From the `acpx/` directory:

```sh
cargo build --workspace
```

Produces `target/debug/acpx-server` and `target/debug/acpx-selftest`
(plus every crate's lib). Use `cargo build --workspace --release` for an
optimized binary.

## Minimal run (native/unmanaged mode)

No config file is required for a single default backend:

```sh
ACPX_BACKEND_CMD="npx -y @agentclientprotocol/codex-acp@1.1.2" \
ACPX_HTTP_BIND="127.0.0.1:8790" \
  target/debug/acpx-server
```

This starts the HTTP/WS transport on `127.0.0.1:8790` and, since
`acpx-server` also runs the stdio transport concurrently, reads
JSON-RPC requests from its own stdin too (if stdin hits EOF immediately
-- the default when backgrounded with no attached terminal -- the whole
process exits, since the stdio loop closing is treated as a shutdown
signal; keep a FIFO or terminal attached to stdin if you only care about
HTTP/WS, or background it with `< /dev/null &` only once you've
confirmed that's the behavior you want -- see `scripts/self_test.sh`'s
FIFO trick if you need the process to stay up unattended without a real
stdio client).

A client (any HTTP/JSON-RPC client, or `acpx-client`) then talks to
`POST http://127.0.0.1:8790/rpc` or upgrades `GET /ws` for a persistent
connection with live `session/update` streaming.

## Environment variables (`ServerConfig::from_env`, [`config.rs`](../acpx-server/src/config.rs))

| Variable | Default | Purpose |
| --- | --- | --- |
| `ACPX_BACKEND_CMD` | `npx -y @agentclientprotocol/codex-acp@1.1.2` | Space-separated program + args for the default/native-mode backend. |
| `ACPX_DEFAULT_AGENT_ID` | `default` | Agent id that command is registered under. |
| `ACPX_NATIVE_AUTH_METHOD_ID` | unset | Explicit backend `authenticate` method for native/unmanaged sessions. Leave unset to preserve ACPX's no-guessing default; set only when the configured backend requires a known method (for example `api-key` for the OpenHands Codex wrapper). |
| `ACPX_HTTP_BIND` | `127.0.0.1:8790` | HTTP/WS bind address. Loopback only by default -- do not point at a public interface without auth + a TLS-terminating reverse proxy (acpx never terminates TLS itself). |
| `ACPX_AUTH_TOKEN` | unset (no auth) | If set, requires `Authorization: Bearer <token>` on `POST /rpc` and the `GET /ws` upgrade. Empty string is treated as unset. |
| `ACPX_DB_PATH` | unset (no persistence) | sqlite file path for session metadata + transcripts. See [`architecture.md`](./architecture.md)'s "Persistence and restart recovery". |
| `ACPX_MASTER_KEYRING_PATH` | `<ACPX_DB_PATH>.keyring` | Encryption keyring file for the durable secret store (only relevant when `ACPX_DB_PATH` is set). Created with `0600` permissions on first use. See [`architecture.md`](./architecture.md)'s "Durable secret and configuration store". |
| `ACPX_MASTER_KEYRING_ROTATE` | unset | Set to `1` on a given startup to rotate the master key and re-encrypt every persisted secret once. Not a schedule. |
| `ACPX_CONFIG_FILE` | unset | Path to a startup provisioning JSON file (providers/central MCP servers/profiles), applied before either transport accepts requests. Malformed/rejected file fails startup outright. |

### Session lifecycle and retention

See [`architecture.md`](./architecture.md)'s "Retention, idle expiry,
and the lifecycle reaper" for what each of these actually does.

| Variable | Default | Purpose |
| --- | --- | --- |
| `ACPX_MAX_SESSIONS_TOTAL` | `128` | Daemon-wide live session cap. |
| `ACPX_MAX_SESSIONS_PER_TENANT` | `16` | Per-tenant live session cap. |
| `ACPX_SESSION_IDLE_TTL_SECONDS` | `1800` (30m) | Idle TTL before an unpinned, not-in-flight session becomes reap-eligible. |
| `ACPX_SESSION_ABSOLUTE_TTL_SECONDS` | `off` | Optional hard age ceiling, independent of activity. `off`/`none` disables it. |
| `ACPX_MAX_PINNED_SESSIONS_PER_TENANT` | `off` (unlimited) | Caps `session/retention/pin` per tenant. `off`/`none` disables the cap. |
| `ACPX_UNBOUND_BRIDGE_SESSION_TTL_SECONDS` | `300` (5m) | TTL for a `/acp` bridge virtual session that never sent its first prompt. |
| `ACPX_CONNECTOR_IDLE_SHUTDOWN_TTL_SECONDS` | `off` | Stops a shared backend process once it has zero referencing live sessions for this long. `off`/`none` disables it (a process, once spawned, only stops via `profiles/delete` or daemon exit). |
| `ACPX_ACTIVE_TURN_DEADLINE_SECONDS` | `off` | Bounds how long a turn may stay in-flight before a best-effort backend `session/cancel` fires and in-flight bookkeeping is cleared. `off`/`none` disables it (an in-flight turn is skipped indefinitely). |
| `ACPX_LIFECYCLE_REAPER_ENABLED` | `1` | Runs the daemon-owned reaper task driving all of the above. |
| `ACPX_LIFECYCLE_REAPER_INTERVAL_SECONDS` | `60` | Reaper tick interval. |

### Startup session recovery

Only meaningful with `ACPX_DB_PATH` set -- see [`architecture.md`](./architecture.md)'s
"Persistence and restart recovery".

| Variable | Default | Purpose |
| --- | --- | --- |
| `ACPX_STARTUP_SESSION_RECOVERY_ENABLED` | `0` | Restore load/resume-capable persisted sessions before either transport starts serving requests. Requires the `startup-session-recovery` build feature. |
| `ACPX_STARTUP_SESSION_RECOVERY_TIMEOUT_SECONDS` | `30` | Per-session recovery timeout; a session that exceeds it is marked `recovery_failed` and its recovery backend process is stopped. |
| `ACPX_STARTUP_SESSION_RECOVERY_CONCURRENCY` | `4` | Bounded concurrency for recovering distinct connectors in parallel. |
| `ACPX_STARTUP_SESSION_RECOVERY_FAIL_FAST` | `0` | `1` aborts startup entirely on the first recovery failure instead of continuing best-effort. |

### Multi-tenant, process isolation, and admin

| Variable | Default | Purpose |
| --- | --- | --- |
| `ACPX_TENANT_ALLOWLIST` | unset (any tenant) | Comma-separated allowed `X-Acpx-Tenant` values; a disallowed tenant is rejected before any session work. |
| `ACPX_AUTH_TENANT_TOKENS` | unset | Per-tenant bearer tokens (`tenant:token,tenant:token`), binding tenant identity to the authenticated caller rather than trusting a self-declared header. |
| `ACPX_TENANT_PROCESS_ISOLATION` | `0` | One dedicated backend process per (profile, tenant) pair instead of one shared process per profile. |
| `ACPX_SESSION_PROCESS_ISOLATION` | `0` | One dedicated backend process per *session* (profile-backed sessions only) instead of sharing a process. |
| `ACPX_ADMIN_BIND` | unset (admin surface off) | Separate bind address for operator-only routes (custom-agent CRUD, etc.) -- see [`architecture.md`](./architecture.md)'s "Admin surface". |
| `ACPX_ADMIN_TOKEN` | unset | Bearer token for `ACPX_ADMIN_BIND` routes. A distinct token from `ACPX_AUTH_TOKEN` on purpose -- never interchangeable. |

### ACP compatibility bridge

| Variable | Default | Purpose |
| --- | --- | --- |
| `ACPX_ACP_BRIDGE_ENABLED` | `0` | Mounts `/acp/rpc`, `/acp/ws`, `GET /acp/models` on the same HTTP/WS listener. Requires `ACPX_ACP_BRIDGE_CONFIG_FILE`. |
| `ACPX_ACP_BRIDGE_CONFIG_FILE` | required when enabled | Path to the bridge's model list (below). |

See "ACP compatibility bridge setup (OpenHands, Zed)" below for a full
worked example.

## Provisioning file (`ACPX_CONFIG_FILE`)

Schema (`ProvisioningFile`/`ProfileEntry`,
[`provisioning.rs`](../acpx-server/src/provisioning.rs)),
`deny_unknown_fields` on both top-level and profile entries (a typo'd key
fails startup loudly rather than being silently ignored):

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

Notes:

- `providers`/`mcp_servers` entries are forwarded as-is through
  `Router::dispatch`'s own `mcp_servers/create`/provider-registration
  path -- same validation a client's own call would get, not a separate
  code path.
- A profile's secret is given as **either** `secret` (raw value, inline
  in the file -- discouraged, since the file itself then becomes a
  secret) **or** `secret_env` (name of an env var read at load time --
  preferred; lets the file be committed/templated while the actual
  value comes from systemd/k8s/whatever secret manager populates the
  process's environment). Setting both is a startup error.
- Once provisioned, a client selects a profile via `session/new`'s
  `_acpx.profile` field. Omitting it entirely stays in native/unmanaged
  mode against `ACPX_DEFAULT_AGENT_ID`, regardless of what's provisioned.

## ACP compatibility bridge setup (OpenHands, Zed)

For a strict ACP client that expects a plain agent with a small, fixed
model list and supports ACP model discovery -- **Zed** and **OpenHands**
both qualify -- run one shared daemon with the bridge enabled, then point
the client at a small stdio-to-WS forwarder (`acpx-acp-bridge`) instead
of `acpx-server` directly. See [`architecture.md`](./architecture.md)'s
"The `/acp` compatibility bridge" for how model selection and virtual
sessions work; this section is the runnable version.

1. **Bridge model config** (`acpx-bridge-config.json`) -- one entry per
   selectable model, each naming a real registered/native `agent_id`:

   ```json
   {
     "default_model": "claude/haiku",
     "models": [
       {"id": "claude/haiku", "name": "Claude Haiku", "agent_id": "claude-acp", "model_id": "haiku"},
       {"id": "codex/default", "name": "Codex", "agent_id": "codex-acp", "model_id": "default"}
     ]
   }
   ```

2. **Start the shared daemon** with the bridge enabled:

   ```sh
   ACPX_HTTP_BIND=127.0.0.1:8790 \
   ACPX_ACP_BRIDGE_ENABLED=1 \
   ACPX_ACP_BRIDGE_CONFIG_FILE=/path/to/acpx-bridge-config.json \
     target/release/acpx-server
   ```

   `GET http://127.0.0.1:8790/acp/models` should now list both models
   (secret-free: id/name/agent id/availability only).

3. **Zed**: point a `CustomAgentServer`/`context_server` entry (or Zed's
   generic ACP custom-agent settings) at `acpx-acp-bridge`:

   ```sh
   ACPX_ACP_BRIDGE_URL=ws://127.0.0.1:8790/acp/ws \
     target/release/acpx-acp-bridge
   ```

   Verified end-to-end against a real Zed checkout and a real
   `claude-agent-acp` backend -- see
   `../memory/acpx/gen/plans/acpx-acp-compatibility/reports/zed-e2e-verification.md`
   for the exact harness, commands, and results (including two real
   ACPX bugs this verification found and fixed).

4. **OpenHands**: use `scripts/openhands-acpx-bridge.sh` as the
   `ACPAgentSettings.acp_command` (`acp_server="custom"`) -- it never
   spawns its own `acpx-server`, only forwards stdio to the already-running
   shared daemon's `/acp/ws`:

   ```sh
   ACPX_ACP_BRIDGE_URL=ws://127.0.0.1:8790/acp/ws \
   ACPX_ACP_BRIDGE_BIN=/path/to/acpx-acp-bridge \
     scripts/openhands-acpx-bridge.sh
   ```

   For OpenHands's per-conversation (no shared daemon, no model picker)
   integration instead, use `scripts/openhands-acpx-claude.sh` /
   `scripts/openhands-acpx-codex.sh` -- native/unmanaged mode, one
   disposable `acpx-server` per conversation, no bridge involved. See
   each script's own header comment for the tradeoff.

## Verifying a fresh setup

`scripts/self_test.sh` is the fastest way to confirm a build actually
works end-to-end without touching any real backend agent or API key: it
builds the workspace, boots a real `acpx-server` against a trivial
synthetic stand-in backend, and runs the `acpx-selftest` CLI against it
over real HTTP.

```sh
cd acpx
./scripts/self_test.sh
```

Prints a final `PASS`/`FAIL` line and exits with `acpx-selftest`'s own
exit code. See [`development.md`](./development.md) for the full test
suite (including opt-in tests against real `claude`/`codex` CLI
adapters) and how to run each layer individually.
