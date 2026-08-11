package daemon

import (
	"log/slog"
	"os"
	"path/filepath"
	"testing"
	"time"

	"snapshotd/internal/config"
)

func newTestDaemon(t *testing.T, binPath string) *Daemon {
	t.Helper()
	cfg := config.Config{
		HomeDir:         t.TempDir(),
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          filepath.Join(t.TempDir(), "run"),
		SnapshotBinPath: binPath,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 3 * time.Second
	t.Cleanup(func() { _ = d.Close() })
	return d
}
