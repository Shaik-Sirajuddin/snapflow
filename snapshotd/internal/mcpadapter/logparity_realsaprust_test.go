package mcpadapter_test

// TestMCPAdapter_LogParity_RealChildAndDaemonLogs is the final, explicit
// re-verification of this thread's original goal: that an MCP-driven edit
// produces two independently-checkable, correlatable pieces of real
// evidence --
//
//  1. the daemon's own "mcp sap edit call" slog line (daemon.go's
//     ForwardSAP, config.LogDir-independent -- this is whatever slog
//     handler the Daemon was constructed with), proving an MCP client
//     asked for a specific edit on a specific session; and
//  2. the real Qt/C++ child process's own log file under config.LogDir
//     (procmgr.go's per-instance "<instanceID>.log" stdout/stderr
//     capture), proving the actual sap_ffi.cpp/Shotcut process really
//     started and ran -- not a mock, not just a successful RPC response.
//
// Unlike the other *_realsaprust_test.go files in this package (which
// prove individual RPC behaviors), this test's only claim is about the
// *observability* plumbing itself: with a real config.LogDir set and a
// real file-backed slog.Logger (not the /dev/null- or os.Stderr-backed
// loggers the other tests use, which can't be grepped back), both logs
// must exist on disk after a session and both must contain independently
// verifiable evidence of the same edit.
import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"

	"github.com/mark3labs/mcp-go/server"
)

func TestMCPAdapter_LogParity_RealChildAndDaemonLogs(t *testing.T) {
	binPath := realSapRustBinary(t)

	homeDir := t.TempDir()
	logDir := filepath.Join(homeDir, "logs")
	cfg := config.Config{
		HomeDir:         homeDir,
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          filepath.Join(t.TempDir(), "run"),
		LogDir:          logDir,
		SnapshotBinPath: binPath,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	daemonLogPath := filepath.Join(homeDir, "daemon.log")
	daemonLogFile, err := os.Create(daemonLogPath)
	if err != nil {
		t.Fatalf("create daemon log file: %v", err)
	}
	defer daemonLogFile.Close()
	// A real file-backed logger, not os.Stderr/io.Discard like the other
	// tests use -- this is what makes ForwardSAP's "mcp sap edit call"
	// lines actually grep-able from disk afterward, exactly as they would
	// be for `snapshotd serve > daemon.log 2>&1` in production.
	logger := slog.New(slog.NewTextHandler(daemonLogFile, &slog.HandlerOptions{Level: slog.LevelInfo}))

	d, err := daemon.New(cfg, logger)
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "log-parity"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch real sap-rust: %v", err)
	}
	if pi.Status != "ready" {
		t.Fatalf("expected ready status, got %s", pi.Status)
	}
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), pi.ID) })

	mcpServer := mcpadapter.New(d)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	agent := newMCPAgent(t, ctx, testServer.URL+"/sse")
	defer agent.Close()

	sel := agent.sapCall("project.select", map[string]any{"projectId": proj.ID})
	if sel["projectId"] != proj.ID {
		t.Fatalf("project.select: expected projectId %s, got %+v", proj.ID, sel)
	}
	// A real mutation: edit.addTrack is what the child-log assertion
	// below relies on -- it goes through MultitrackModel, whose Qt
	// signals sap_ffi.cpp's event-emission path listens on (confirmed via
	// scripts/debug-crossfade-repro.py's own log tail: addTrack/appendClip
	// calls print "[sap_ffi] event: {\"type\":\"edit.changed\"}" to
	// stderr, unlike e.g. generator.createTitle which doesn't touch the
	// multitrack directly).
	track := agent.sapCall("edit.addTrack", map[string]any{"kind": "video"})
	if _, ok := track["index"]; !ok {
		t.Fatalf("edit.addTrack should return an index, got %+v", track)
	}

	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}

	// --- 1. The daemon's own MCP-edit-call log ---
	daemonLogBytes, err := os.ReadFile(daemonLogPath)
	if err != nil {
		t.Fatalf("read daemon log: %v", err)
	}
	daemonLog := string(daemonLogBytes)
	if !strings.Contains(daemonLog, "mcp sap edit call") {
		t.Fatalf("daemon log should contain ForwardSAP's per-call log line, log=%s", daemonLog)
	}
	if !strings.Contains(daemonLog, "method=edit.addTrack") {
		t.Fatalf("daemon log should record the edit.addTrack method name, log=%s", daemonLog)
	}
	if !strings.Contains(daemonLog, "sessionId=") {
		t.Fatalf("daemon log should record the calling session's id, log=%s", daemonLog)
	}

	// --- 2. The real child process's own sap_ffi.cpp/stderr log ---
	childLogPath := filepath.Join(logDir, pi.ID+".log")
	childLogBytes, err := os.ReadFile(childLogPath)
	if err != nil {
		t.Fatalf("read per-instance child log %s: %v", childLogPath, err)
	}
	childLog := string(childLogBytes)
	if len(strings.TrimSpace(childLog)) == 0 {
		t.Fatalf("per-instance child log %s should be non-empty (real sap_ffi.cpp/Qt stderr output)", childLogPath)
	}
	// sap_ffi.cpp's own event-notification diagnostic (see sap_ffi.cpp's
	// qWarning/stderr emission around edit.changed) is the real proof this
	// is the actual C++ process reacting to the edit, not just the Go
	// daemon believing the RPC round-tripped successfully.
	if !strings.Contains(childLog, "edit.changed") {
		t.Fatalf("per-instance child log should contain the real sap_ffi.cpp edit.changed event line, log=%s", childLog)
	}

	t.Logf(
		"log parity confirmed: daemon log %s has the MCP edit-call line (edit.addTrack) and child log %s has the real sap_ffi.cpp process's own edit.changed event, %d bytes total",
		daemonLogPath, childLogPath, len(childLog),
	)
}
