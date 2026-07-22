package acpxmgr

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestWriteConfig(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "acpx-config.json")
	if err := WriteConfig(path, "http://127.0.0.1:7777/mcp", "default"); err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	s := string(raw)
	// The registered profile is deliberately NOT named after agentID (see
	// WriteConfig's own doc comment: a profile name equal to agentID gets
	// silently picked up by acpx-core's native-session fallback lookup and
	// breaks ACPX_NATIVE_AUTH_METHOD_ID). agentID surfaces as the
	// profile's "agent_id" field instead, not its "name".
	for _, part := range []string{`"name": "snapshotd"`, `"url": "http://127.0.0.1:7777/mcp"`, `"agent_id": "default"`} {
		if !strings.Contains(s, part) {
			t.Fatalf("missing %q in:\n%s", part, s)
		}
	}
}

func TestEnsureAdminTokenIsStableAcrossCalls(t *testing.T) {
	dir := t.TempDir()
	cfg := Config{ConfigPath: filepath.Join(dir, "acpx-config.json")}

	first, err := ensureAdminToken(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if len(first) == 0 {
		t.Fatal("expected a non-empty generated token")
	}

	second, err := ensureAdminToken(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if first != second {
		t.Fatalf("expected the same token on a second call (persisted, not regenerated), got %q then %q", first, second)
	}

	// A real client (panel-rust) reads this exact file independently --
	// prove it round-trips through a plain file read too, not just
	// through ensureAdminToken's own re-read path.
	raw, err := os.ReadFile(adminTokenPath(cfg))
	if err != nil {
		t.Fatal(err)
	}
	if strings.TrimSpace(string(raw)) != first {
		t.Fatalf("token file content %q does not match generated token %q", raw, first)
	}
}

func TestMcpHTTPURL(t *testing.T) {
	cases := map[string]string{
		"127.0.0.1:7777":            "http://127.0.0.1:7777/mcp",
		":7777":                     "http://127.0.0.1:7777/mcp",
		"http://127.0.0.1:7777":     "http://127.0.0.1:7777/mcp",
		"http://127.0.0.1:7777/sse": "http://127.0.0.1:7777/mcp",
		"http://127.0.0.1:7777/mcp": "http://127.0.0.1:7777/mcp",
	}
	for in, want := range cases {
		if got := McpHTTPURL(in); got != want {
			t.Errorf("McpHTTPURL(%q)=%q want %q", in, got, want)
		}
	}
}
