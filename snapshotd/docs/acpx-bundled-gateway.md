# Bundled acpx-server under `snapshotd serve`

When `SNAPSHOTD_ACPX_ENABLED=1` (default **on** when an `acpx-server` binary
is discoverable, or forced via env), `snapshotd serve` can own a long-lived
child **acpx-server** and provision a default config that registers the
**snapshotd MCP** centrally.

Panel settings JSON (profile name / harness) does **not** replace this file;
it only chooses which provisioned **profile name** to use on `session/new`.

## Process tree

```
snapshotd serve
├── SDP control.sock
├── MCP SSE + Streamable HTTP  (e.g. 127.0.0.1:7777 → /sse and /mcp)
└── acpx-server child (optional)
      env: ACPX_CONFIG_FILE, ACPX_HTTP_BIND, ACPX_DB_PATH, …
```

## Manual Phase 0 (no daemon spawn)

1. Start snapshotd:

   ```sh
   export SNAPSHOTD_HOME=/tmp/snapshotd-acpx-demo
   export SNAPSHOTD_MCP_SSE_ADDR=127.0.0.1:7777
   snapshotd serve
   ```

2. Copy and edit the example config so the MCP URL matches the live bind:

   ```sh
   cp snapshotd/docs/acpx-config.snapshotd.example.json /tmp/acpx-config.json
   # edit url if SNAPSHOTD_MCP_SSE_ADDR is not 127.0.0.1:7777
   ```

3. Start acpx:

   ```sh
   export ACPX_CONFIG_FILE=/tmp/acpx-config.json
   export ACPX_HTTP_BIND=127.0.0.1:8790
   export ACPX_DB_PATH=/tmp/snapshotd-acpx-demo/acpx.sqlite3
   # ACPX_BACKEND_CMD=…  # real agent or mock
   acpx-server
   ```

4. Checks (logs + curl only):

   ```sh
   curl -sf "http://127.0.0.1:8790/health"
   # POST /rpc mcp_servers/list → entry name "snapshotd"
   ```

## Env (Phase 1 child)

| Variable | Default | Meaning |
|----------|---------|---------|
| `SNAPSHOTD_ACPX_ENABLED` | `1` if bin found else `0` | Spawn acpx child |
| `SNAPSHOTD_ACPX_BIN` | discover `acpx-server` next to snapshotd / checkout | Binary path |
| `SNAPSHOTD_ACPX_HTTP_BIND` | `127.0.0.1:8790` | Child HTTP/WS bind |
| `SNAPSHOTD_ACPX_CONFIG` | `$SNAPSHOTD_HOME/acpx-config.json` | Written at serve time |

Panel clients:

```sh
export RUI_ACPX_CODEX_URL=http://127.0.0.1:8790
export RUI_ACPX_CLAUDE_URL=http://127.0.0.1:8790
```

See `memory/rui/gen/plans/chat-panel/snapshotd-bundled-acpx-gateway.md`.
