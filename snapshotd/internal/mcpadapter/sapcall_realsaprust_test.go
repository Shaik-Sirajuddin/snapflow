package mcpadapter_test

import (
	"context"
	"encoding/json"
	"log/slog"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"

	mcpclient "github.com/mark3labs/mcp-go/client"
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"
)

// realSapRustBinary locates the real, production child binary these
// integration tests need: the Qt/`real_ffi`-linked `shotcut` binary (see
// shotcut/CMakeLists.txt's corrosion_import_crate(... FEATURES real_ffi)
// and sap-rust/README.md's "Real FFI" section), under
// shotcut/build*/src/shotcut relative to this repo's root. This is
// deliberately NOT the standalone `sap-rust/target/{debug,release}/sap-rust`
// binary any more -- since the MltBackend removal, that binary only ever
// runs MockBackend (no real ffprobe/melt), which cannot back the real
// file.export/file.probe assertions several tests in this package make. If
// no such build exists yet, the test is skipped, not failed -- this
// package's `go test ./...` must not require a full Qt build to exist. In
// this checkout it does exist, so this test actually proves the full MCP
// -> daemon -> sapproxy -> real headless Shotcut/FfiBackend chain end to
// end, including real mutated project state and real fanned-out
// notifications delivered over a live SSE connection.
func realSapRustBinary(t *testing.T) string {
	t.Helper()
	repoRoot := filepath.Join("..", "..", "..")
	matches, err := filepath.Glob(filepath.Join(repoRoot, "shotcut", "build*", "src", "shotcut"))
	if err == nil {
		for _, candidate := range matches {
			if info, statErr := os.Stat(candidate); statErr == nil && !info.IsDir() {
				abs, absErr := filepath.Abs(candidate)
				if absErr != nil {
					t.Fatalf("abs path: %v", absErr)
				}
				return abs
			}
		}
	}
	t.Skip("real Qt/real_ffi shotcut binary not found under shotcut/build*/src/shotcut; run `cmake -S shotcut -B shotcut/build-real-ffi -G Ninja && ninja -C shotcut/build-real-ffi` first to run this integration test")
	return ""
}

// TestMCPAdapter_SapCallTool_RealSapRust_EndToEnd stands up a real daemon
// core (registry + process manager + generic SAP proxy) backed by the real
// sap-rust binary, serves it over MCP/SSE exactly as `snapshotd serve`
// does, connects a real MCP SSE client, and drives project.select then a
// real mutation through the generic "sap.call" tool -- confirming the
// result is real (mutated) MockBackend state, not a stub, and that the
// resulting edit.changed notification is fanned out to the SSE client as a
// real "sap.notification" MCP notification.
func TestMCPAdapter_SapCallTool_RealSapRust_EndToEnd(t *testing.T) {
	binPath := realSapRustBinary(t)

	debugHome := "/tmp/sap-debug-home"
	_ = os.RemoveAll(debugHome)
	cfg := config.Config{
		HomeDir:         debugHome,
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          filepath.Join(t.TempDir(), "run"),
		SnapshotBinPath: binPath,
	}
	cfg.LogDir = filepath.Join(cfg.HomeDir, "logs")
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := daemon.New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 60 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "mcp-e2e"})
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

	mcpServer := mcpadapter.New(d)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()

	var notifMu sync.Mutex
	var notifs []mcp.JSONRPCNotification
	c.OnNotification(func(n mcp.JSONRPCNotification) {
		notifMu.Lock()
		defer notifMu.Unlock()
		notifs = append(notifs, n)
	})

	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	// project.select over the real generic proxy.
	selectReq := mcp.CallToolRequest{}
	selectReq.Params.Name = "sap.call"
	selectReq.Params.Arguments = map[string]any{
		"method": "project.select",
		"params": map[string]any{"projectId": proj.ID},
	}
	selectRes, err := c.CallTool(ctx, selectReq)
	if err != nil {
		t.Fatalf("call sap.call(project.select): %v", err)
	}
	if selectRes.IsError {
		t.Fatalf("unexpected error result: %+v", toolResultText(selectRes))
	}
	selectState := decodeToolResultJSON(t, selectRes)
	if selectState["projectId"] != proj.ID {
		t.Fatalf("expected real ProjectState.projectId == %s, got %+v", proj.ID, selectState)
	}

	// edit.addTrack -- a real mutation against the real (Mock) backend.
	addReq := mcp.CallToolRequest{}
	addReq.Params.Name = "sap.call"
	addReq.Params.Arguments = map[string]any{
		"method": "edit.addTrack",
		"params": map[string]any{"kind": "video"},
	}
	addRes, err := c.CallTool(ctx, addReq)
	if err != nil {
		t.Fatalf("call sap.call(edit.addTrack): %v", err)
	}
	if addRes.IsError {
		t.Fatalf("unexpected error result: %+v", toolResultText(addRes))
	}
	track := decodeToolResultJSON(t, addRes)
	if track["kind"] != "video" {
		t.Fatalf("expected real track result with kind=video, got %+v", track)
	}

	// edit.listTracks -- read back the real, persisted mutation.
	listReq := mcp.CallToolRequest{}
	listReq.Params.Name = "sap.call"
	listReq.Params.Arguments = map[string]any{"method": "edit.listTracks", "params": map[string]any{}}
	listRes, err := c.CallTool(ctx, listReq)
	if err != nil {
		t.Fatalf("call sap.call(edit.listTracks): %v", err)
	}
	if listRes.IsError {
		t.Fatalf("unexpected error result: %+v", toolResultText(listRes))
	}
	tracks := decodeArrayResult(t, toolResultText(listRes))
	if len(tracks) != 1 || tracks[0]["kind"] != "video" {
		t.Fatalf("expected the real, previously-added track to be listed, got %+v", tracks)
	}

	// Real fanned-out notification, delivered over the live SSE connection
	// as an MCP "sap.notification" wrapping sap-rust's edit.changed.
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		notifMu.Lock()
		n := len(notifs)
		notifMu.Unlock()
		if n > 0 {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	notifMu.Lock()
	defer notifMu.Unlock()
	if len(notifs) == 0 {
		t.Fatalf("expected at least one sap.notification delivered over SSE")
	}
	found := false
	for _, n := range notifs {
		if n.Method == "sap.notification" {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected a sap.notification frame, got: %+v", notifs)
	}

	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}
}

func toolResultText(res *mcp.CallToolResult) string {
	for _, c := range res.Content {
		if tc, ok := c.(mcp.TextContent); ok {
			return tc.Text
		}
	}
	return ""
}

func decodeToolResultJSON(t *testing.T, res *mcp.CallToolResult) map[string]any {
	t.Helper()
	var out map[string]any
	if err := json.Unmarshal([]byte(toolResultText(res)), &out); err != nil {
		t.Fatalf("unmarshal tool result JSON: %v (raw: %s)", err, toolResultText(res))
	}
	return out
}
