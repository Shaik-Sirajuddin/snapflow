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
	for _, part := range []string{`"name": "snapshotd"`, `"url": "http://127.0.0.1:7777/mcp"`, `"name": "default"`} {
		if !strings.Contains(s, part) {
			t.Fatalf("missing %q in:\n%s", part, s)
		}
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
