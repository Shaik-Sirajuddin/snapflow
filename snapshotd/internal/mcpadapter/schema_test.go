package mcpadapter_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"testing"

	"snapshotd/internal/mcpadapter"
)

// TestCommittedSchemaMatchesCurrentTools fails the moment
// docs/schema/snapshotd-mcp.schema.json drifts from what
// mcpadapter.AllTools currently builds -- same drift-guard pattern as
// acpx's committed_openrpc_file_matches_current_method_registry (see
// acpx/acpx-proto/tests/openrpc_test.rs), adapted to Go: no process-spawn
// or PATH dependency, runs as fast as any other unit test.
//
// Both sides are compared as generic JSON (map[string]any / []any), not
// unmarshaled into mcp.Tool: some tools set a raw, non-object output
// schema (e.g. markers.next/prev's {"oneOf": [...]}) that mcp-go's typed
// ToolOutputSchema cannot represent, so round-tripping through it would
// silently drop that field and produce false negatives.
func TestCommittedSchemaMatchesCurrentTools(t *testing.T) {
	committedPath := filepath.Join("..", "..", "docs", "schema", "snapshotd-mcp.schema.json")
	committedRaw, err := os.ReadFile(committedPath)
	if err != nil {
		t.Fatalf("failed to read %s: %v -- run scripts/gen_mcp_schema.sh first", committedPath, err)
	}
	var committedDoc struct {
		Tools []json.RawMessage `json:"tools"`
	}
	if err := json.Unmarshal(committedRaw, &committedDoc); err != nil {
		t.Fatalf("committed schema file is not valid JSON: %v", err)
	}

	current := mcpadapter.AllTools(&fakeHandler{})
	currentRaw, err := json.Marshal(current)
	if err != nil {
		t.Fatalf("marshaling current tools: %v", err)
	}
	var currentTools []json.RawMessage
	if err := json.Unmarshal(currentRaw, &currentTools); err != nil {
		t.Fatalf("re-unmarshaling current tools: %v", err)
	}

	currentNorm := normalizeToolsByName(t, currentTools)
	committedNorm := normalizeToolsByName(t, committedDoc.Tools)

	if currentNorm != committedNorm {
		t.Fatalf("%s is stale -- a tool changed without regenerating it. Run: bash scripts/gen_mcp_schema.sh", committedPath)
	}
}

func TestClosedEnumsAreAdvertised(t *testing.T) {
	tools := mcpadapter.AllTools(&fakeHandler{})
	byName := make(map[string]map[string]any, len(tools))
	for _, tool := range tools {
		raw, err := json.Marshal(tool)
		if err != nil {
			t.Fatalf("marshal %s: %v", tool.Name, err)
		}
		var decoded map[string]any
		if err := json.Unmarshal(raw, &decoded); err != nil {
			t.Fatalf("decode %s: %v", tool.Name, err)
		}
		byName[tool.Name] = decoded
	}
	assertEnum := func(toolName, property string, want []any) {
		t.Helper()
		tool, ok := byName[toolName]
		if !ok {
			t.Fatalf("missing tool %s", toolName)
		}
		schema, ok := tool["inputSchema"].(map[string]any)
		if !ok {
			t.Fatalf("%s has no inputSchema", toolName)
		}
		properties, ok := schema["properties"].(map[string]any)
		if !ok {
			t.Fatalf("%s inputSchema has no properties", toolName)
		}
		field, ok := properties[property].(map[string]any)
		if !ok {
			t.Fatalf("%s has no %s property", toolName, property)
		}
		got, ok := field["enum"].([]any)
		if !ok {
			t.Fatalf("%s.%s has no enum: %#v", toolName, property, field)
		}
		if string(mustJSON(t, got)) != string(mustJSON(t, want)) {
			t.Fatalf("%s.%s enum = %s, want %s", toolName, property, mustJSON(t, got), mustJSON(t, want))
		}
	}
	assertEnum("project.create", "projectType", []any{"folder", "file"})
	assertEnum("playback.getFrame", "format", []any{"jpeg", "png"})
	assertEnum("subtitles.setStyle", "style", []any{"normal", "italic"})
	assertEnum("subtitles.setStyle", "valign", []any{"top", "middle", "bottom"})
	assertEnum("subtitles.setStyle", "halign", []any{"left", "center", "right"})
}

func mustJSON(t *testing.T, value any) []byte {
	t.Helper()
	raw, err := json.Marshal(value)
	if err != nil {
		t.Fatalf("marshal JSON value: %v", err)
	}
	return raw
}

// normalizeToolsByName decodes each tool into a generic map, sorts by
// name, and re-encodes deterministically so two structurally-identical
// tool sets compare equal regardless of source key/slice ordering.
func normalizeToolsByName(t *testing.T, raws []json.RawMessage) string {
	t.Helper()
	tools := make([]map[string]any, 0, len(raws))
	for _, raw := range raws {
		var m map[string]any
		if err := json.Unmarshal(raw, &m); err != nil {
			t.Fatalf("decoding tool: %v", err)
		}
		tools = append(tools, m)
	}
	sort.Slice(tools, func(i, j int) bool {
		return tools[i]["name"].(string) < tools[j]["name"].(string)
	})
	out, err := json.MarshalIndent(tools, "", "  ")
	if err != nil {
		t.Fatalf("re-encoding normalized tools: %v", err)
	}
	return string(out)
}
