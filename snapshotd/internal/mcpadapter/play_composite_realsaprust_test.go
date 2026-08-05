package mcpadapter_test

import (
	"bytes"
	"context"
	"encoding/base64"
	"image"
	_ "image/jpeg"
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

// TestMCPAdapter_PlayCompositeProfileImageSequence_RealSapRust drives the
// new playback / track-composite / canvas-profile / image-sequence MCP
// surface through the same daemon + real headless Shotcut chain as
// sapcall_realsaprust_test.go. Skips when the Qt real_ffi binary is missing
// or predates the new FFI symbols (methods return SAP errors).
func TestMCPAdapter_PlayCompositeProfileImageSequence_RealSapRust(t *testing.T) {
	binPath := realSapRustBinary(t)

	// Short paths: Unix domain socket paths are length-limited (see other
	// realsaprust tests using /tmp/... rather than deep t.TempDir trees).
	home := "/tmp/sap-play-composite"
	_ = os.RemoveAll(home)
	t.Cleanup(func() { _ = os.RemoveAll(home) })
	cfg := config.Config{
		HomeDir:         home,
		ProjectsRoot:    filepath.Join(home, "projects"),
		RunDir:          filepath.Join(home, "run"),
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

	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "play-composite"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	if _, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: proj.ID}); err != nil {
		t.Fatalf("launch: %v", err)
	}

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

	sapCall := func(method string, args map[string]any) map[string]any {
		t.Helper()
		req := mcp.CallToolRequest{}
		req.Params.Name = method
		req.Params.Arguments = args
		res, err := c.CallTool(ctx, req)
		if err != nil {
			t.Fatalf("%s transport: %v", method, err)
		}
		if res.IsError {
			msg := toolResultText(res)
			// Only skip when the SAP server literally lacks the method
			// (old binary not rebuilt with new FFI). Do not match generic
			// "failed"/"not found" — those hide real regressions.
			if isSAPMethodNotFound(msg) {
				t.Skipf("%s unavailable on this binary (rebuild shotcut real_ffi): %s", method, msg)
			}
			t.Fatalf("%s error: %s", method, msg)
		}
		return decodeToolResultJSON(t, res)
	}

	_ = sapCall("project.open", map[string]any{"projectId": proj.ID})

	// --- Canvas / profile ---
	prof := sapCall("project.setProfile", map[string]any{
		"width": 1280, "height": 720, "frameRateNum": 25, "frameRateDen": 1,
	})
	if w, ok := asInt(prof["width"]); !ok || w != 1280 {
		prof = sapCall("project.getProfile", map[string]any{})
		if w, ok := asInt(prof["width"]); !ok || w != 1280 {
			t.Fatalf("expected width 1280 after setProfile, got %+v", prof)
		}
	}
	if h, ok := asInt(prof["height"]); !ok || h != 720 {
		t.Fatalf("expected height 720, got %+v", prof)
	}

	// --- Tracks + composite ---
	_ = sapCall("edit.addTrack", map[string]any{"kind": "video"})
	t1 := sapCall("edit.addTrack", map[string]any{"kind": "video"})
	idx1, ok := asInt(t1["index"])
	if !ok {
		t.Fatalf("addTrack missing index: %+v", t1)
	}
	updated := sapCall("edit.setTrackProperties", map[string]any{
		"trackIndex": idx1,
		"composite":  true,
		"blendMode":  "0",
	})
	if comp, ok := updated["composite"].(bool); ok && !comp {
		t.Fatalf("expected composite true on overlay track, got %+v", updated)
	}

	// --- Image sequence (3 PNGs) ---
	seqDir := t.TempDir()
	// Minimal valid 2x2 RGB PNG.
	png := []byte{
		0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
		0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02,
		0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a, 0x73, 0x00, 0x00, 0x00,
		0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0xc0,
		0xc0, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x27, 0x0a, 0x00,
		0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
	}
	for _, name := range []string{"frame_001.png", "frame_002.png", "frame_003.png"} {
		if err := os.WriteFile(filepath.Join(seqDir, name), png, 0o644); err != nil {
			t.Fatalf("write png: %v", err)
		}
	}
	seq := sapCall("playlist.appendImageSequence", map[string]any{
		"path": filepath.Join(seqDir, "frame_001.png"),
		"ttl":  1,
	})
	if dur, ok := asInt(seq["durationFrames"]); ok && dur < 1 {
		t.Fatalf("expected positive durationFrames for image sequence, got %+v", seq)
	}

	// --- Playback transport ---
	_ = sapCall("playback.seek", map[string]any{"frame": 0})
	_ = sapCall("playback.play", map[string]any{"speed": 1.0})
	st := sapCall("playback.getState", map[string]any{})
	if len(st) > 0 {
		if _, ok := st["position"]; !ok {
			t.Fatalf("playback.getState missing position: %+v", st)
		}
	}
	_ = sapCall("playback.pause", map[string]any{})
	_ = sapCall("playback.stop", map[string]any{})

	frame := sapCall("playback.getFrame", map[string]any{"frame": 0, "format": "jpeg"})
	if b64, ok := frame["data"].(string); ok && b64 != "" {
		raw, err := base64.StdEncoding.DecodeString(b64)
		if err != nil {
			t.Fatalf("decode frame: %v", err)
		}
		img, _, err := image.Decode(bytes.NewReader(raw))
		if err == nil {
			b := img.Bounds()
			if b.Dx() <= 0 || b.Dy() <= 0 {
				t.Fatalf("bad frame size %v", b)
			}
		}
	}
}

func asInt(v any) (int, bool) {
	switch n := v.(type) {
	case float64:
		return int(n), true
	case int:
		return n, true
	case int64:
		return int(n), true
	default:
		return 0, false
	}
}

// isSAPMethodNotFound reports whether a tool error is specifically
// JSON-RPC method-not-found for a missing SAP method on an old binary.
// Matches sap-rust's -32601 wording without swallowing other failures.
func isSAPMethodNotFound(msg string) bool {
	lower := strings.ToLower(msg)
	if strings.Contains(msg, "-32601") {
		return true
	}
	// e.g. "method not found: project.setProfile" / "unknown method project.setProfile"
	if strings.Contains(lower, "method not found") {
		return true
	}
	if strings.Contains(lower, "unknown method") {
		return true
	}
	return false
}
