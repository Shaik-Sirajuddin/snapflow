package acpxmgr

import (
	"context"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
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
	for _, part := range []string{`"name": "snapflow"`, `"url": "http://127.0.0.1:7777/mcp"`, `"agent_id": "default"`} {
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

// TestStartPortBumpIsDiscoverableViaBindFile is a regression test for the
// real bug: when the requested ACPX_HTTP_BIND is already occupied,
// acpxmgr.Start silently walks to the next free port (nextAvailableTCPBind)
// and health-checks *that* address internally -- but until this fix,
// nothing external could learn the resolved address, so a caller (like
// vnc_worktree.sh) that hard-polls the originally-requested port spins
// forever against a dead port and declares a perfectly healthy
// daemon-managed acpx-server "never became healthy". This proves the new
// acpx-http-bind discovery file reflects the real, resolved address.
func TestStartPortBumpIsDiscoverableViaBindFile(t *testing.T) {
	if _, err := exec.LookPath("python3"); err != nil {
		t.Skip("python3 not available to stand in for acpx-server")
	}

	// Occupy a real port first so Start is forced to bump off of it --
	// this is the "stale process / another worktree's leaked child /
	// concurrent-worktree race" scenario from production.
	occupied, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer occupied.Close()
	requestedBind := occupied.Addr().String()

	dir := t.TempDir()
	fakeServer := writeFakeAcpxServer(t, dir)

	cfg := Config{
		BinPath:        fakeServer,
		HttpBind:       requestedBind,
		ConfigPath:     filepath.Join(dir, "snapshotd-home", "acpx-config.json"),
		McpURL:         "http://127.0.0.1:1/mcp",
		DefaultAgentID: "default",
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	mgr, err := Start(ctx, cfg)
	if err != nil {
		t.Fatalf("Start: %v", err)
	}
	defer mgr.Stop()

	if mgr.HTTPBind() == requestedBind {
		t.Fatalf("expected Start to bump off the occupied requested bind %q, got the same address back", requestedBind)
	}

	raw, err := os.ReadFile(httpBindFilePath(cfg))
	if err != nil {
		t.Fatalf("acpx-http-bind file was not written: %v", err)
	}
	fileBind := strings.TrimSpace(string(raw))
	if fileBind != mgr.HTTPBind() {
		t.Fatalf("acpx-http-bind file contains %q, want the actual resolved bind %q -- an external poller (vnc_worktree.sh) reading this file would still be looking at the wrong address", fileBind, mgr.HTTPBind())
	}
	if fileBind == requestedBind {
		t.Fatalf("acpx-http-bind file still contains the originally-requested (occupied) bind %q; the whole point is that it must reflect the bump", fileBind)
	}
}

// writeFakeAcpxServer writes a minimal executable script that binds
// $ACPX_HTTP_BIND and answers any GET with 200, standing in for the real
// acpx-server binary (which this test suite has no reason to build) --
// good enough to exercise acpxmgr's spawn + bind-resolution + health-poll
// path exactly the way the real binary would.
func writeFakeAcpxServer(t *testing.T, dir string) string {
	t.Helper()
	path := filepath.Join(dir, "fake-acpx-server.py")
	script := `#!/usr/bin/env python3
import http.server, os

bind = os.environ["ACPX_HTTP_BIND"]
host, port = bind.rsplit(":", 1)


class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()

    def log_message(self, *a):
        pass


http.server.HTTPServer((host, int(port)), H).serve_forever()
`
	if err := os.WriteFile(path, []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	return path
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
