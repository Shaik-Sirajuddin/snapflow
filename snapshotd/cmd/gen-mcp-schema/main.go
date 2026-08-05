// Command gen-mcp-schema prints the full snapshotd MCP tool schema
// (name/description/inputSchema for every tool this package can build, the
// same shape a live "tools/list" call returns) as JSON on stdout. It calls
// mcpadapter.AllTools against a no-op Handler -- no daemon, no sockets, no
// running process -- so the schema can be regenerated offline and kept in
// sync with the code via the drift-guard test in
// internal/mcpadapter/schema_test.go, mirroring acpx's
// gen-openapi/gen-openrpc pattern (see acpx/scripts/gen_openrpc.sh).
//
// This includes the same typed per-method tools (edit.*, playlist.*, filter.*,
// audio.*, generator.*, subtitles.*, file.*, jobs.*, playback.*, notes.*,
// markers.*, recent.*) that mcpadapter.New registers on the live server.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"sort"

	"github.com/mark3labs/mcp-go/mcp"

	"snapshotd/internal/mcpadapter"
	"snapshotd/internal/sapproxy"
)

// noopHandler satisfies mcpadapter.Handler purely for schema extraction --
// none of its methods are ever invoked since tool handlers aren't called
// while building the tool list.
type noopHandler struct{}

func (noopHandler) Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error) {
	return nil, nil
}

func (noopHandler) ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error) {
	return nil, nil
}

func (noopHandler) UnbindSession(sessionID string) {}

func main() {
	tools := mcpadapter.AllTools(noopHandler{})
	sort.Slice(tools, func(i, j int) bool { return tools[i].Name < tools[j].Name })

	doc := struct {
		ServerInfo struct {
			Name    string `json:"name"`
			Version string `json:"version"`
		} `json:"serverInfo"`
		Tools []mcp.Tool `json:"tools"`
	}{}
	doc.ServerInfo.Name = "snapshotd"
	doc.ServerInfo.Version = "0.1.0"
	doc.Tools = tools

	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	if err := enc.Encode(doc); err != nil {
		fmt.Fprintln(os.Stderr, "gen-mcp-schema:", err)
		os.Exit(1)
	}
}
