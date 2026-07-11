package procmgr

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"snapshotd/internal/registry"
)

// buildFixture compiles the throwaway testdata/fixture program (a stand-in
// for the real sap-rust binary, which is developed independently and is not
// assumed to exist here) into a temp directory and returns its path.
func buildFixture(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	out := filepath.Join(dir, "fixture-bin")
	cmd := exec.Command("go", "build", "-o", out, "snapshotd/internal/procmgr/testdata/fixture")
	cmd.Env = os.Environ()
	if outBytes, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("building test fixture binary: %v\n%s", err, outBytes)
	}
	return out
}

func openTestRegistry(t *testing.T) *registry.Registry {
	t.Helper()
	reg, err := registry.Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatalf("open registry: %v", err)
	}
	t.Cleanup(func() { _ = reg.Close() })
	return reg
}

func TestLaunch_SpawnsFixtureAndWiresEnvVars(t *testing.T) {
	fixtureBin := buildFixture(t)
	reg := openTestRegistry(t)

	if err := reg.CreateProject(&registry.Project{
		ID:      "proj-1",
		RootDir: t.TempDir(),
		Status:  "active",
	}); err != nil {
		t.Fatalf("create project: %v", err)
	}

	runDir := t.TempDir()
	fixtureOut := filepath.Join(t.TempDir(), "fixture-out.txt")

	mgr := New(reg, fixtureBin, runDir)
	mgr.ConnectTimeout = 3 * time.Second

	// Manager.Launch always appends os.Environ() plus the SAP vars; smuggle
	// SNAPSHOT_FIXTURE_OUT in via the process's own environment so the
	// fixture picks it up too.
	t.Setenv("SNAPSHOT_FIXTURE_OUT", fixtureOut)

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	pi, err := mgr.Launch(ctx, "proj-1", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("launch: %v", err)
	}
	defer mgr.Close(pi.ID)

	if pi.Status != registry.StatusReady {
		t.Fatalf("expected status ready, got %s", pi.Status)
	}
	if pi.SocketPath == "" {
		t.Fatalf("expected non-empty socket path")
	}
	if pi.PID <= 0 {
		t.Fatalf("expected positive pid, got %d", pi.PID)
	}

	// Give the fixture a moment to have written its env-var dump (it writes
	// before it starts listening, and Launch already confirmed the listener
	// is up, so this should already be present).
	data, err := os.ReadFile(fixtureOut)
	if err != nil {
		t.Fatalf("reading fixture output: %v", err)
	}
	content := string(data)
	if !strings.Contains(content, "socket="+pi.SocketPath) {
		t.Fatalf("fixture did not see expected socket path, got: %s", content)
	}
	if !strings.Contains(content, "headless=1") {
		t.Fatalf("fixture did not see SNAPSHOT_HEADLESS=1, got: %s", content)
	}
	if strings.Contains(content, "token=\n") {
		t.Fatalf("fixture saw an empty SNAPSHOT_SAP_TOKEN, expected a generated value: %s", content)
	}

	// Persisted row should be retrievable via List/Health.
	rows, err := mgr.List()
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	found := false
	for _, r := range rows {
		if r.ID == pi.ID {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected launched instance in List(), got %+v", rows)
	}

	gotPi, healthy, err := mgr.Health(pi.ID)
	if err != nil {
		t.Fatalf("health: %v", err)
	}
	if !healthy {
		t.Fatalf("expected instance to be healthy")
	}
	if gotPi.ID != pi.ID {
		t.Fatalf("unexpected instance id from Health: %s", gotPi.ID)
	}
}

func TestLaunch_MissingBinary_ReturnsCleanError(t *testing.T) {
	reg := openTestRegistry(t)
	if err := reg.CreateProject(&registry.Project{ID: "proj-2", RootDir: t.TempDir(), Status: "active"}); err != nil {
		t.Fatalf("create project: %v", err)
	}

	mgr := New(reg, filepath.Join(t.TempDir(), "does-not-exist-binary"), t.TempDir())

	_, err := mgr.Launch(context.Background(), "proj-2", LaunchOptions{})
	if err == nil {
		t.Fatalf("expected error for missing binary")
	}
	if !strings.Contains(err.Error(), "not found") {
		t.Fatalf("expected a clean 'not found' error, got: %v", err)
	}
}

func TestClose_StopsProcessAndMarksClosed(t *testing.T) {
	fixtureBin := buildFixture(t)
	reg := openTestRegistry(t)
	if err := reg.CreateProject(&registry.Project{ID: "proj-3", RootDir: t.TempDir(), Status: "active"}); err != nil {
		t.Fatalf("create project: %v", err)
	}

	mgr := New(reg, fixtureBin, t.TempDir())
	pi, err := mgr.Launch(context.Background(), "proj-3", LaunchOptions{})
	if err != nil {
		t.Fatalf("launch: %v", err)
	}

	if err := mgr.Close(pi.ID); err != nil {
		t.Fatalf("close: %v", err)
	}

	row, err := reg.GetProcessInstance(pi.ID)
	if err != nil {
		t.Fatalf("get instance: %v", err)
	}
	if row.Status != registry.StatusClosed {
		t.Fatalf("expected status closed, got %s", row.Status)
	}
}
