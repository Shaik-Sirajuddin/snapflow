# acpx ACP-compatibility bridge setup (OpenHands, Zed)

A quick-start guide for pointing a strict ACP client -- one that expects
a plain agent endpoint with a small, fixed, discoverable model list
rather than acpx's own profile/provider machinery -- at acpx. **Zed**
and **OpenHands** both support ACP model discovery and are the two
clients this bridge has been verified against.

For acpx's own native surface (profiles, providers, retention
administration) instead, see
[`README.native.md`](./README.native.md). For full reference
(every method, every env var, how model binding/virtual sessions work
under the hood), see [`docs/architecture.md`](./docs/architecture.md)'s
"The `/acp` compatibility bridge" and [`docs/setup.md`](./docs/setup.md)'s
"ACP compatibility bridge setup" sections -- this file is a short path
to a first working setup, not a replacement for either.

## How it fits together

```
client (Zed / OpenHands)
   |  stdio ACP
   v
acpx-acp-bridge   (small forwarder binary; one per client connection)
   |  ws://.../acp/ws
   v
acpx-server       (one shared daemon; owns the real backend processes)
   |
   v
claude-agent-acp / codex-acp / ...   (real backend adapter processes)
```

One daemon serves every client connection. A bridge session is
*virtual*: nothing spawns a real backend process until the client's
first `session/prompt`, and a session that only ever calls
`GET /acp/models` never spawns one at all.

## 1. Build (or use the release binary)

```sh
git clone https://github.com/Shaik-Sirajuddin/multi_media_main.git
cd multi_media_main/acpx
cargo build --release -p acpx-server -p acpx-bridge
# binaries: target/release/acpx-server, target/release/acpx-acp-bridge
```

## 2. Write the bridge model config

`acpx-bridge-config.json` -- one entry per selectable model, each naming
a real registered/native `agent_id` (see `agents/list` against a running
daemon, or the bundled adapter registry, for valid ids):

```json
{
  "default_model": "claude/haiku",
  "models": [
    {"id": "claude/haiku", "name": "Claude Haiku", "agent_id": "claude-acp", "model_id": "haiku"},
    {"id": "codex/default", "name": "Codex", "agent_id": "codex-acp", "model_id": "default"}
  ]
}
```

## 3. Start the shared daemon with the bridge enabled

```sh
ACPX_HTTP_BIND=127.0.0.1:8790 \
ACPX_ACP_BRIDGE_ENABLED=1 \
ACPX_ACP_BRIDGE_CONFIG_FILE=/path/to/acpx-bridge-config.json \
  target/release/acpx-server
```

Confirm model discovery is live:

```sh
curl http://127.0.0.1:8790/acp/models
```

Should list both `claude/haiku` and `codex/default` (id/name/agent id/
availability only -- no secrets).

## 4. Point Zed at it

Configure a custom ACP agent server that runs `acpx-acp-bridge`:

```sh
ACPX_ACP_BRIDGE_URL=ws://127.0.0.1:8790/acp/ws \
  target/release/acpx-acp-bridge
```

This has been verified end-to-end against a real Zed checkout
(`common_e2e_tests!`, unmodified assertions) and a real
`claude-agent-acp` backend: prompt/response, live streaming updates,
tool-call status transitions, thread teardown, and interactive
permission-request forwarding all pass. See
`memory/acpx/gen/plans/acpx-acp-compatibility/reports/zed-e2e-verification.md`
in this repository for the full harness, exact commands, and results
(including two real acpx bugs this verification found and fixed).

## 5. Point OpenHands at it

**Shared-daemon mode** (recommended -- one daemon, model discovery,
matches the diagram above): use `scripts/openhands-acpx-bridge.sh` as
the `ACPAgentSettings.acp_command` (`acp_server="custom"`). It never
spawns its own `acpx-server`, only forwards stdio to the already-running
daemon's `/acp/ws`:

```sh
ACPX_ACP_BRIDGE_URL=ws://127.0.0.1:8790/acp/ws \
ACPX_ACP_BRIDGE_BIN=/path/to/target/release/acpx-acp-bridge \
  scripts/openhands-acpx-bridge.sh
```

**Per-conversation mode** (no shared daemon, no model picker, native/
unmanaged mode only): `scripts/openhands-acpx-claude.sh` /
`scripts/openhands-acpx-codex.sh` each spawn their own disposable
`acpx-server` per conversation instead. Use this if you don't need
model discovery and prefer conversation-scoped isolation over a shared
daemon. See each script's own header comment for the tradeoff, and
`acpx/tests/openhands_integration/README.md` for the real, credentialed
end-to-end coverage against both modes.

## 6. Tuning virtual-session limits (optional)

- `max_virtual_sessions_per_tenant` (bridge config file field, default
  unlimited) caps how many virtual bridge sessions one tenant may hold
  at once.
- `ACPX_UNBOUND_BRIDGE_SESSION_TTL_SECONDS` (default 5 minutes) reaps a
  virtual session that picked a model (or didn't) but never sent a
  first prompt, without ever spawning a backend process for it.
