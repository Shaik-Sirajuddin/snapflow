// End-to-end test for the `snapshotd mcp *` CLI subcommands: real
// `snapshotd serve` subprocess, driven via further real `snapshotd`
// subprocess invocations plus a real HTTP client, so it actually proves
// auth is enforced over the wire -- not just that the supervisor's Go API
// behaves (that's internal/mcpsupervisor's own unit tests).
package main

import (
	"encoding/json"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

// freePort asks the OS for an unused TCP port on 127.0.0.1, then releases
// it immediately -- good enough for a test that binds it again within
// milliseconds, avoiding a hardcoded port that might collide.
func freePort(t *testing.T) int {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("finding a free port: %v", err)
	}
	defer ln.Close()
	return ln.Addr().(*net.TCPAddr).Port
}

func TestCLI_MCPStatusRestartAuthInstallConfig(t *testing.T) {
	fixtureBin := buildBinary(t, "snapshotd/internal/procmgr/testdata/fixture", "fixture-bin")
	snapshotdBin := buildBinary(t, ".", "snapshotd-bin")

	homeDir := t.TempDir()
	mcpPort := freePort(t)

	serveCmd := exec.Command(snapshotdBin, "serve")
	serveCmd.Env = append(os.Environ(),
		"SNAPSHOTD_HOME="+homeDir,
		"SNAPSHOT_BIN_PATH="+fixtureBin,
		"SNAPSHOTD_ACPX_ENABLED=false",
		"SNAPSHOTD_MCP_SSE_ADDR=127.0.0.1:0",
	)
	var serveOut strings.Builder
	serveCmd.Stdout = &serveOut
	serveCmd.Stderr = &serveOut
	if err := serveCmd.Start(); err != nil {
		t.Fatalf("starting snapshotd serve: %v", err)
	}
	t.Cleanup(func() {
		_ = serveCmd.Process.Kill()
		_ = serveCmd.Wait()
	})

	controlSock := filepath.Join(homeDir, "control.sock")
	waitForSocket(t, controlSock, 5*time.Second, &serveOut)

	// status on a fresh daemon: listening, auth disabled.
	statusOut := runCLI(t, snapshotdBin, homeDir, "mcp", "status")
	var status struct {
		Listening   bool
		AuthEnabled bool
	}
	if err := json.Unmarshal([]byte(statusOut), &status); err != nil {
		t.Fatalf("unmarshal status %q: %v", statusOut, err)
	}
	if !status.Listening || status.AuthEnabled {
		t.Fatalf("unexpected initial status: %+v (raw %q)", status, statusOut)
	}

	// restart to a non-loopback bind without auth: refused.
	nonLoopback := addrOn("0.0.0.0", mcpPort)
	if out, err := runCLIErr(t, snapshotdBin, homeDir, "mcp", "restart", "--bind", nonLoopback); err == nil {
		t.Fatalf("expected restart to %s to be refused without auth, got success: %q", nonLoopback, out)
	} else if !strings.Contains(out, "auth") {
		t.Fatalf("expected refusal message to mention auth, got: %q", out)
	}

	// still listening on the original loopback addr (refusal didn't tear anything down).
	statusOut = runCLI(t, snapshotdBin, homeDir, "mcp", "status")
	if err := json.Unmarshal([]byte(statusOut), &status); err != nil {
		t.Fatalf("unmarshal status %q: %v", statusOut, err)
	}
	if !status.Listening {
		t.Fatalf("expected listener to remain up after refused restart, got: %q", statusOut)
	}

	// set auth.
	authOut := runCLI(t, snapshotdBin, homeDir, "mcp", "auth", "set", "--user", "alice", "--password", "s3cret")
	if !strings.Contains(authOut, "alice") {
		t.Fatalf("expected auth set output to mention the user, got: %q", authOut)
	}

	// restart to the same non-loopback bind now succeeds, and warns on
	// stderr that this is still plaintext HTTP (stdout/stderr are merged by
	// the runCLI test helper for diagnostics; a real caller piping only
	// stdout gets clean JSON, which is what's asserted below).
	restartOut := runCLI(t, snapshotdBin, homeDir, "mcp", "restart", "--bind", nonLoopback)
	if !strings.Contains(restartOut, "WARNING") || !strings.Contains(restartOut, "not encrypted") {
		t.Fatalf("expected a plaintext-HTTP warning on a successful non-loopback restart, got: %q", restartOut)
	}
	if err := json.Unmarshal([]byte(restartOut[strings.IndexByte(restartOut, '{'):]), &status); err != nil {
		t.Fatalf("unmarshal restart output %q: %v", restartOut, err)
	}
	if !status.Listening || !status.AuthEnabled {
		t.Fatalf("expected listening+authEnabled after restart, got: %+v (raw %q)", status, restartOut)
	}

	// unauthenticated HTTP request to the now-0.0.0.0-bound endpoint: 401.
	// Dial via 127.0.0.1 (0.0.0.0 listens on all interfaces, including loopback).
	url := "http://" + addrOn("127.0.0.1", mcpPort) + "/mcp"
	client := &http.Client{Timeout: 2 * time.Second}
	resp, err := client.Get(url)
	if err != nil {
		t.Fatalf("unauthenticated GET %s: %v", url, err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("expected 401 without credentials, got %d", resp.StatusCode)
	}

	// authenticated request succeeds (not 401).
	req, _ := http.NewRequest(http.MethodGet, url, nil)
	req.SetBasicAuth("alice", "s3cret")
	resp, err = client.Do(req)
	if err != nil {
		t.Fatalf("authenticated GET %s: %v", url, err)
	}
	resp.Body.Close()
	if resp.StatusCode == http.StatusUnauthorized {
		t.Fatalf("expected auth to pass with correct credentials, got 401")
	}

	// install-config get reflects the same credentials.
	installOut := runCLI(t, snapshotdBin, homeDir, "mcp", "install-config", "get")
	var install struct {
		AuthUser      string
		AuthPassword  string
		SSEURL        string `json:"sseUrl"`
		StreamableURL string `json:"streamableUrl"`
	}
	if err := json.Unmarshal([]byte(installOut), &install); err != nil {
		t.Fatalf("unmarshal install-config %q: %v", installOut, err)
	}
	if install.AuthUser != "alice" || install.AuthPassword != "s3cret" {
		t.Fatalf("expected install-config to echo set credentials, got: %+v (raw %q)", install, installOut)
	}
	if install.SSEURL == "" || install.StreamableURL == "" {
		t.Fatalf("expected install-config to include endpoint URLs, got: %q", installOut)
	}

	if out, err := runCLIErr(t, snapshotdBin, homeDir, "stop"); err != nil {
		t.Fatalf("snapshotd stop: %v\n%s", err, out)
	}
}

func addrOn(host string, port int) string {
	return net.JoinHostPort(host, strconv.Itoa(port))
}
