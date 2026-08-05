#!/usr/bin/env bash
# Regenerates docs/schema/snapshotd-mcp.schema.json from the actual
# mcpadapter.New() tool registration (see internal/mcpadapter/mcpadapter.go)
# via cmd/gen-mcp-schema, which builds the real MCP server against a no-op
# Handler and dumps server.MCPServer.ListTools() -- no daemon, no sockets,
# no running process required. Run this after adding/removing/changing an
# MCP tool, then run `go test ./internal/mcpadapter/...` -- schema_test.go
# fails the build if the committed file and the current registration
# disagree. Mirrors acpx/scripts/gen_openrpc.sh's drift-guard pattern.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/docs/schema/snapshotd-mcp.schema.json"

mkdir -p "$(dirname "$out")"
(cd "$repo_root" && go run ./cmd/gen-mcp-schema) >"$out"

echo "wrote $out"
