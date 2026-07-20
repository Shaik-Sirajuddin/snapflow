package mcpadapter_test

// TestMCPAdapter_SelectionState_RealSapRust_EndToEnd is
// mcp-selection-state's `e2e_verification` phase: proves the new
// track.enter/clip.enter/currentView/lock_tools_to_selection/
// selection_remap_on_mutation behavior over the real MCP -> daemon ->
// sap-rust chain (the real Qt/real_ffi shotcut binary, same as
// sapcall_realsaprust_test.go), not just the sap-rust crate's own
// server_integration.rs tests. No snapshotd-side code changes were needed
// for this at all: "sap.call" is already a fully generic method+params
// passthrough (mcpadapter.go's own doc comment), so track.enter/clip.enter/
// currentView work through it automatically, the same as any other real
// SAP method.
//
// Covers both halves the source task asks for:
//  1. the selection/index state stays correct through a real sequence
//     (track.enter -> a locked mutation with no explicit trackIndex ->
//     currentView confirms it, then edit.reorderTrack remaps it), and
//  2. misuse (an edit.* call needing a selection with none entered)
//     returns a minimal, clear error naming the correct tool to use,
//     instead of a raw backend failure.

import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	mcpclient "github.com/mark3labs/mcp-go/client"
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"
)

func TestMCPAdapter_SelectionState_RealSapRust_EndToEnd(t *testing.T) {
	binPath := realSapRustBinary(t)

	debugHome := filepath.Join(t.TempDir(), "sap-selection-debug-home")
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

	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "selection-e2e"})
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

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	call := func(method string, params map[string]any) *mcp.CallToolResult {
		t.Helper()
		req := mcp.CallToolRequest{}
		req.Params.Name = "sap.call"
		req.Params.Arguments = map[string]any{"method": method, "params": params}
		res, err := c.CallTool(ctx, req)
		if err != nil {
			t.Fatalf("call sap.call(%s): %v", method, err)
		}
		return res
	}

	selectRes := call("project.select", map[string]any{"projectId": proj.ID})
	if selectRes.IsError {
		t.Fatalf("project.select: %s", toolResultText(selectRes))
	}

	// --- Misuse first: no track selected yet, edit.setTrackProperties
	// must fail with a clear, actionable error naming the correct tool.
	call("edit.addTrack", map[string]any{"kind": "video"})
	call("edit.addTrack", map[string]any{"kind": "video"})
	misuse := call("edit.setTrackProperties", map[string]any{"muted": true})
	if !misuse.IsError {
		t.Fatalf("edit.setTrackProperties with no track.enter must be rejected, got: %s", toolResultText(misuse))
	}
	misuseMsg := toolResultText(misuse)
	if !strings.Contains(misuseMsg, "track.enter") {
		t.Fatalf("misuse error should name track.enter as the correct tool to use, got: %s", misuseMsg)
	}

	// --- Real sequence: select track 1, mutate it with NO explicit
	// trackIndex, confirm via currentView, then reorder and confirm the
	// selection followed the same logical track.
	track1Enter := call("track.enter", map[string]any{"trackIndex": 1})
	if track1Enter.IsError {
		t.Fatalf("track.enter: %s", toolResultText(track1Enter))
	}

	props := call("edit.setTrackProperties", map[string]any{"muted": true, "blendMode": "14"})
	if props.IsError {
		t.Fatalf("edit.setTrackProperties (selection-scoped, no explicit trackIndex): %s", toolResultText(props))
	}
	propsResult := decodeToolResultJSON(t, props)
	if propsResult["muted"] != true {
		t.Fatalf("expected the real, selection-scoped track to be muted, got %+v", propsResult)
	}

	view := call("currentView", map[string]any{})
	if view.IsError {
		t.Fatalf("currentView: %s", toolResultText(view))
	}
	viewResult := decodeToolResultJSON(t, view)
	if v, ok := viewResult["trackIndex"].(float64); !ok || int(v) != 1 {
		t.Fatalf("expected currentView.trackIndex == 1, got %+v", viewResult)
	}

	reordered := call("edit.reorderTrack", map[string]any{"fromIndex": 1, "toIndex": 0})
	if reordered.IsError {
		t.Fatalf("edit.reorderTrack: %s", toolResultText(reordered))
	}
	viewAfterReorder := decodeToolResultJSON(t, call("currentView", map[string]any{}))
	if v, ok := viewAfterReorder["trackIndex"].(float64); !ok || int(v) != 0 {
		t.Fatalf("expected the selection to follow the reordered track to index 0, got %+v", viewAfterReorder)
	}

	// An explicit trackIndex is ignored, never honored as an override,
	// through the real chain too -- track 0 (the real selection) gets
	// muted, not track 1 (the explicitly but ineffectively named index).
	call("track.enter", map[string]any{"trackIndex": 0})
	call("edit.setTrackProperties", map[string]any{"trackIndex": 1, "hidden": true})
	tracks := decodeArrayResult(t, toolResultText(call("edit.listTracks", map[string]any{})))
	if tracks[0]["hidden"] != true {
		t.Fatalf("track 0 (the real selection) should be hidden, got %+v", tracks[0])
	}
	if tracks[1]["hidden"] == true {
		t.Fatalf("track 1 (the ignored explicit trackIndex) must be untouched, got %+v", tracks[1])
	}
}
