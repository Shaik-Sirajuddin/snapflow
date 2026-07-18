# ACPX Verification Report

## Deterministic Gate

Run from `acpx/`:

```bash
cargo test --workspace --no-fail-fast
cargo build -p acpx-server -p acpx-bridge --bins
python3 -m unittest tests.bridge_integration.test_stdio_bridge -v
python3 -m py_compile \
  tests/openhands_integration/openhands_sdk_driver.py \
  tests/openhands_integration/test_openhands_shared_bridge.py
```

Result: passed on this workspace.

## Covered Behavior

| Area | Evidence |
| --- | --- |
| Native ACP lifecycle | Capacity, tenant limits, recovery `load`/`resume`, retention candidate selection, pin behavior, safe backend close before reaper removal |
| Native transport | Stdio, HTTP, WebSocket, auth, tenant isolation, cancellation, concurrent agents, persistence, fork/load/resume |
| Strict ACP bridge | `/acp/rpc`, `/acp/ws`, public model aliases, lazy bind, default model, selection validation, virtual fork IDs |
| Bridge transport | Stdio-to-WebSocket binary, bearer token and tenant forwarding, black-box bridge daemon round trip |
| Live delivery | Bound bridge sessions receive standalone `session/update` frames with virtual IDs before final prompt responses |
| Schema | Generated OpenRPC/OpenAPI/wire-schema consistency tests and real Claude schema conversation |
| OpenHands | Shared bridge wrapper, conversation-secret injection helpers, and a live OpenHands -> bridge -> Claude conversation |

## External Verification

The real Claude ambient schema conversation was executed successfully:

```bash
ACPX_LIVE_TEST_AMBIENT=1 \
  cargo test -p acpx-server --test real_ambient_multi_agent_test \
  ambient_claude_only_conversation_conforms_to_generated_schema \
  -- --ignored --nocapture
```

The real Codex ambient test is intentionally bounded to 120 seconds. The
installed `codex-acp` adapter starts, but its `chat-gpt` authentication flow
does not complete headlessly on this host. This is an adapter/runtime
limitation, not treated as a passing ACPX verification.

The OpenHands shared-bridge test was executed against the running OpenHands
agent-server on port `18000` and an isolated bridge-enabled ACPX daemon
exposing `claude/haiku`. It passed after normalizing omitted `mcpServers`,
returning ACP-required `configOptions` after model selection, and flushing
first-turn buffered `session/update` frames as standard ACP notifications.

Re-run it with an operator-started bridge-enabled daemon and model aliases:

```bash
ACPX_OPENHANDS_BRIDGE_URL=ws://127.0.0.1:8790/acp/ws \
ACPX_OPENHANDS_BRIDGE_MODELS=claude/sonnet \
uv run --with openhands-sdk==1.29.0 --with pytest \
  pytest tests/openhands_integration/test_openhands_shared_bridge.py -v \
  --openhands-host http://127.0.0.1:8000
```

Optional variables:

- `ACPX_OPENHANDS_BRIDGE_TOKEN`
- `ACPX_OPENHANDS_BRIDGE_TENANT`
