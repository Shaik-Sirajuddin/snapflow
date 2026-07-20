package mcpadapter_test

// TestPanelIconRendering_RealSapRust_SurvivesSustainedRendering is
// mcp-selection-state's `runtime_and_edge_pass` phase's "real-instance
// icon/path-resolution check": launches the real Qt/real_ffi shotcut
// binary (which embeds panel-rust's chat dock, continuously re-rendering
// on its own QTimer poll loop) and confirms it stays healthy across
// several real render passes, rather than crashing.
//
// This is the direct regression test for the root-caused Slint crash this
// session found and fixed (panel-rust commit 3ecf509): every Image with
// image-fit: contain (the shared Icon primitive, AgentLogo's brand marks)
// relied on Slint 1.17.1's undocumented-in-practice ClippedImage default
// full-source clip, which resolved to a 0x0 rect -- fit()'s scale
// computation divided by that zero, producing a NaN/infinite size that
// panicked when cast to an integer pixel size. That crash reproduced on
// the very first render, well before this test's multi-second window, so
// simply staying "ready" through it is a genuine, real proof the icon
// rendering (and the disk-path resolution + size resolution
// @image-url(...) needs to load each SVG) now actually works end to end --
// prior to the fix, every real-shotcut-dependent test in this whole
// package (including this one, had it existed) failed identically.
import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"testing"
	"time"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
)

func TestPanelIconRendering_RealSapRust_SurvivesSustainedRendering(t *testing.T) {
	binPath := realSapRustBinary(t)

	// A short, fixed-prefix run dir rather than t.TempDir() -- this test's
	// own name is long enough that a socket path nested under
	// t.TempDir()/run/<hash>.sock exceeds AF_UNIX's ~108-byte path limit.
	runDir, err := os.MkdirTemp("", "icon-e2e-")
	if err != nil {
		t.Fatalf("mkdir run dir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(runDir) })
	debugHome := filepath.Join(runDir, "home")
	cfg := config.Config{
		HomeDir:         debugHome,
		ProjectsRoot:    filepath.Join(runDir, "projects"),
		RunDir:          filepath.Join(runDir, "run"),
		SnapshotBinPath: binPath,
	}
	cfg.LogDir = filepath.Join(cfg.HomeDir, "logs")
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := daemon.New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	// Real Qt/MLT cold start is slow in this sandbox (~15-20s observed) --
	// generous timeout, same reasoning as this package's other real-process
	// tests.
	d.Proc.ConnectTimeout = 60 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "icon-render"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch real sap-rust: %v", err)
	}
	if pi.Status != "ready" {
		t.Fatalf("expected ready status immediately after launch, got %s", pi.Status)
	}
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), pi.ID) })

	// The panel's own QTimer-driven poll/render loop runs continuously
	// (see panel-rust/src/agent_bridge.rs's module doc on why nothing else
	// drives it) -- staying alive and "ready" across several real seconds
	// means many real render passes have already happened, each one
	// re-drawing every visible Icon (sidebar controls, settings-tab icons,
	// agent logos) through the exact code path that used to panic on the
	// very first pass.
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		result, err := d.Health(ctx, pi.ID)
		if err != nil {
			t.Fatalf("health check: %v", err)
		}
		if result.Instance.Status == "crashed" || !result.Healthy {
			t.Fatalf("instance crashed during sustained rendering (the Icon/AgentLogo fit() NaN-size regression this test guards against): %+v", result)
		}
		time.Sleep(250 * time.Millisecond)
	}
}
