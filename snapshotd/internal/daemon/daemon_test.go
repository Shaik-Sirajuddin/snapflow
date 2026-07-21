package daemon

import (
	"context"
	"encoding/json"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"testing"
	"time"

	"snapshotd/internal/config"
	"snapshotd/internal/registry"
)

// buildFixture compiles the same throwaway fixture binary used by
// internal/procmgr's tests, so the daemon-level integration test also runs
// against a real (if trivial) listening child process instead of the
// not-yet-built sap-rust binary.
func buildFixture(t *testing.T) string {
	t.Helper()
	name := "fixture-bin"
	if runtime.GOOS == "windows" {
		// See procmgr_test.go's buildFixture: Windows' exec.LookPath needs
		// a PATHEXT-listed extension even for an absolute path.
		name += ".exe"
	}
	out := filepath.Join(t.TempDir(), name)
	cmd := exec.Command("go", "build", "-o", out, "snapshotd/internal/procmgr/testdata/fixture")
	if outBytes, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("building fixture: %v\n%s", err, outBytes)
	}
	return out
}

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

func TestDaemon_ProjectAndLaunchLifecycle(t *testing.T) {
	fixtureBin := buildFixture(t)
	d := newTestDaemon(t, fixtureBin)
	ctx := context.Background()

	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "demo"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	if proj.MltFileName != registry.DefaultMltFileName {
		t.Fatalf("expected default mlt filename, got %q", proj.MltFileName)
	}
	if _, err := os.Stat(proj.RootDir); err != nil {
		t.Fatalf("expected project folder to exist: %v", err)
	}

	projects, err := d.ListProjects(ctx)
	if err != nil || len(projects) != 1 {
		t.Fatalf("expected 1 listed project, got %d (err=%v)", len(projects), err)
	}

	pi, err := d.Launch(ctx, LaunchParams{ProjectID: proj.ID, Headless: boolPtr(true)})
	if err != nil {
		t.Fatalf("launch: %v", err)
	}
	if pi.Status != registry.StatusReady {
		t.Fatalf("expected ready status, got %s", pi.Status)
	}

	instances, err := d.List(ctx)
	if err != nil || len(instances) != 1 {
		t.Fatalf("expected 1 instance, got %d (err=%v)", len(instances), err)
	}

	hr, err := d.Health(ctx, pi.ID)
	if err != nil {
		t.Fatalf("health: %v", err)
	}
	if !hr.Healthy {
		t.Fatalf("expected healthy instance")
	}

	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}
	row, err := d.Reg.GetProcessInstance(pi.ID)
	if err != nil || row.Status != registry.StatusClosed {
		t.Fatalf("expected closed status, got %+v (err=%v)", row, err)
	}

	if err := d.DeleteProject(ctx, proj.ID); err != nil {
		t.Fatalf("delete project: %v", err)
	}
	if _, err := os.Stat(proj.RootDir); err != nil {
		t.Fatalf("expected project folder to remain on disk after delete: %v", err)
	}
}

func TestDaemon_Dispatch_RoutesAllDaemonMethods(t *testing.T) {
	fixtureBin := buildFixture(t)
	d := newTestDaemon(t, fixtureBin)
	ctx := context.Background()

	call := func(method string, params any) json.RawMessage {
		raw, _ := json.Marshal(params)
		result, err := d.Dispatch(ctx, method, raw)
		if err != nil {
			t.Fatalf("dispatch %s: %v", method, err)
		}
		out, _ := json.Marshal(result)
		return out
	}

	createOut := call("daemon.createProject", CreateProjectParams{Name: "via-dispatch"})
	var proj registry.Project
	if err := json.Unmarshal(createOut, &proj); err != nil {
		t.Fatalf("unmarshal project: %v", err)
	}

	call("daemon.listProjects", nil)

	launchOut := call("daemon.launch", LaunchParams{ProjectID: proj.ID})
	var pi registry.ProcessInstance
	if err := json.Unmarshal(launchOut, &pi); err != nil {
		t.Fatalf("unmarshal instance: %v", err)
	}

	call("daemon.list", nil)
	call("daemon.health", map[string]string{"instanceId": pi.ID})
	call("daemon.close", map[string]string{"instanceId": pi.ID})
	call("daemon.deleteProject", map[string]string{"projectId": proj.ID})

	if _, err := d.Dispatch(ctx, "daemon.doesNotExist", nil); err == nil {
		t.Fatalf("expected error for unknown method")
	}
}

func boolPtr(b bool) *bool { return &b }
