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
| `ACPX_CONFIG_FILE` | unset | Path to a startup provisioning JSON file (providers/central MCP servers/profiles), applied before either transport accepts requests. Malformed/rejected file fails startup outright. |

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
