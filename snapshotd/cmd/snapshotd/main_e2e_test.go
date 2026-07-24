// End-to-end test for the bare `snapshotd list` / `snapshotd close` CLI
// subcommands: unlike internal/daemon/daemon_test.go's
// TestDaemon_Dispatch_RoutesAllDaemonMethods (which calls Daemon.Dispatch
// in-process), this spawns the real snapshotd binary as `serve`, then drives
// it via further snapshotd subprocess invocations, so it actually exercises
// main.go's argv parsing, output formatting, and process lifecycle -- not
// just the daemon core underneath it.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

// buildBinary compiles a package under this module into a temp dir, mirroring
// internal/daemon/daemon_test.go's buildFixture helper.
func buildBinary(t *testing.T, pkg, name string) string {
	t.Helper()
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	out := filepath.Join(t.TempDir(), name)
	cmd := exec.Command("go", "build", "-o", out, pkg)
	if outBytes, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("building %s: %v\n%s", pkg, err, outBytes)
	}
	return out
}

// runCLI runs the snapshotd binary with the given args against homeDir's
// daemon and returns combined stdout, or fails the test with stderr on error.
func runCLI(t *testing.T, snapshotdBin, homeDir string, args ...string) string {
	t.Helper()
	cmd := exec.Command(snapshotdBin, args...)
	cmd.Env = append(os.Environ(), "SNAPSHOTD_HOME="+homeDir)
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = &out
	if err := cmd.Run(); err != nil {
		t.Fatalf("snapshotd %s: %v\n%s", strings.Join(args, " "), err, out.String())
	}
	return out.String()
}

// runCLIErr is like runCLI but expects a non-zero exit, returning combined
// output instead of failing the test.
func runCLIErr(t *testing.T, snapshotdBin, homeDir string, args ...string) (string, error) {
	t.Helper()
	cmd := exec.Command(snapshotdBin, args...)
	cmd.Env = append(os.Environ(), "SNAPSHOTD_HOME="+homeDir)
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = &out
	err := cmd.Run()
	return out.String(), err
}

func TestCLI_ListAndClose_AgainstRealDaemon(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("cmdStop/serve pidfile+SIGTERM handling in main.go is unix-only in this build")
	}

	fixtureBin := buildBinary(t, "snapshotd/internal/procmgr/testdata/fixture", "fixture-bin")
	snapshotdBin := buildBinary(t, ".", "snapshotd-bin")

	homeDir := t.TempDir()
	projectDir := filepath.Join(t.TempDir(), "e2e-project")
	if err := os.MkdirAll(projectDir, 0o755); err != nil {
		t.Fatalf("mkdir project dir: %v", err)
	}

	serveCmd := exec.CommandContext(context.Background(), snapshotdBin, "serve", "--no-mcp")
	serveCmd.Env = append(os.Environ(),
		"SNAPSHOTD_HOME="+homeDir,
		"SNAPSHOT_BIN_PATH="+fixtureBin,
		"SNAPSHOTD_ACPX_ENABLED=false",
	)
	var serveOut bytes.Buffer
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

	// list on an empty daemon: no error, no output lines.
	out := runCLI(t, snapshotdBin, homeDir, "list")
	if strings.TrimSpace(out) != "" {
		t.Fatalf("expected no instances listed on empty daemon, got: %q", out)
	}

	// launch then list shows the instance.
	launchOut := runCLI(t, snapshotdBin, homeDir, "launch", "--gui", projectDir)
	var launched struct {
		ID     string
		Status string
	}
	if err := json.Unmarshal([]byte(launchOut), &launched); err != nil {
		t.Fatalf("unmarshal launch output %q: %v", launchOut, err)
	}
	if launched.ID == "" {
		t.Fatalf("expected non-empty instance id from launch, got %q", launchOut)
	}

	listOut := runCLI(t, snapshotdBin, homeDir, "list")
	if !strings.Contains(listOut, launched.ID) {
		t.Fatalf("expected list output to contain launched instance id %q, got: %q", launched.ID, listOut)
	}

	// close the instance, then list reflects it's no longer live.
	closeOut := runCLI(t, snapshotdBin, homeDir, "close", launched.ID)
	if !strings.Contains(closeOut, launched.ID) {
		t.Fatalf("expected close output to mention instance id, got: %q", closeOut)
	}

	listOut = runCLI(t, snapshotdBin, homeDir, "list")
	var afterClose []struct {
		ID     string
		Status string
	}
	// Each line of `list` output is one JSON object.
	for _, line := range strings.Split(strings.TrimSpace(listOut), "\n") {
		if line == "" {
			continue
		}
		var inst struct {
			ID     string
			Status string
		}
		if err := json.Unmarshal([]byte(line), &inst); err != nil {
			t.Fatalf("unmarshal list line %q: %v", line, err)
		}
		afterClose = append(afterClose, inst)
	}
	found := false
	for _, inst := range afterClose {
		if inst.ID == launched.ID {
			found = true
			if inst.Status != "closed" {
				t.Fatalf("expected closed instance status \"closed\", got %q", inst.Status)
			}
		}
	}
	if !found {
		t.Fatalf("expected closed instance to still be listed (with closed status), got: %v", afterClose)
	}

	// close with an unknown instanceId errors cleanly (non-zero exit).
	if out, err := runCLIErr(t, snapshotdBin, homeDir, "close", "does-not-exist"); err == nil {
		t.Fatalf("expected error closing unknown instance id, got success: %q", out)
	}

	// tear down via the existing bare `stop` subcommand.
	if out, err := runCLIErr(t, snapshotdBin, homeDir, "stop"); err != nil {
		t.Fatalf("snapshotd stop: %v\n%s", err, out)
	}
}

func waitForSocket(t *testing.T, path string, timeout time.Duration, serveOut fmt.Stringer) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for control socket %s to appear; serve output:\n%s", path, serveOut.String())
}
