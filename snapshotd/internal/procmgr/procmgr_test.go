package procmgr

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"snapshotd/internal/registry"
	"snapshotd/internal/transport"
)

// buildFixture compiles the throwaway testdata/fixture program (a stand-in
// for the real sap-rust binary, which is developed independently and is not
// assumed to exist here) into a temp directory and returns its path.
func buildFixture(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	name := "fixture-bin"
	if runtime.GOOS == "windows" {
		// Windows' exec.LookPath requires a PATHEXT-listed extension even
		// for an absolute path -- without it, starting the built binary
		// fails with "executable file not found in %PATH%" despite the
		// file existing right there.
		name += ".exe"
	}
	out := filepath.Join(dir, name)
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

func TestBundledMeltBinary_FindsAppSiblingWithoutUsingHome(t *testing.T) {
	appDir := t.TempDir()
	binDir := filepath.Join(appDir, "bin")
	if err := os.MkdirAll(binDir, 0o755); err != nil {
		t.Fatalf("create app bin dir: %v", err)
	}
	melt := filepath.Join(binDir, "melt")
	if err := os.WriteFile(melt, []byte("fixture"), 0o755); err != nil {
		t.Fatalf("write bundled melt fixture: %v", err)
	}
	got := bundledMeltBinary(filepath.Join(appDir, "snapflow"))
	if got != melt {
		t.Fatalf("bundledMeltBinary() = %q, want %q", got, melt)
	}
}

func TestAppendBundledMltEnv_UsesAbsoluteBundlePaths(t *testing.T) {
	appDir := t.TempDir()
	bin := filepath.Join(appDir, "snapflow.exe")
	repo := filepath.Join(appDir, "lib", "mlt")
	data := filepath.Join(appDir, "share", "mlt")
	if err := os.MkdirAll(repo, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(data, 0o755); err != nil {
		t.Fatal(err)
	}
	got := appendBundledMltEnv([]string{"PATH=x", "MLT_REPOSITORY=stale"}, bin)
	joined := strings.Join(got, "\n")
	if !strings.Contains(joined, "MLT_REPOSITORY="+repo) || !strings.Contains(joined, "MLT_DATA="+data) {
		t.Fatalf("bundle MLT paths missing: %s", joined)
	}
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

	mgr := New(reg, fixtureBin, runDir, t.TempDir())
	mgr.ConnectTimeout = 3 * time.Second

	// Manager.Launch always appends os.Environ() plus the SAP vars; smuggle
	// SNAPSHOT_FIXTURE_OUT in via the process's own environment so the
	// fixture picks it up too.
	t.Setenv("SNAPSHOT_FIXTURE_OUT", fixtureOut)
	// A stale parent value must never win over the per-instance endpoint
	// exported by Manager.Launch.
	t.Setenv("SNAPSHOT_SAP_ENDPOINT", filepath.Join(t.TempDir(), "stale.sock"))
	t.Setenv("SNAPSHOT_SAP_SOCKET", filepath.Join(t.TempDir(), "stale.sock"))
	t.Setenv("SNAPSHOT_SAP_TOKEN", "stale-token")

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	pi, _, err := mgr.Launch(ctx, "proj-1", LaunchOptions{Headless: true})
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
	if !strings.Contains(content, "endpoint="+pi.SocketPath) {
		t.Fatalf("fixture did not see transport-neutral SAP endpoint, got: %s", content)
	}
	if strings.Contains(content, "stale-token") {
		t.Fatalf("fixture inherited stale SAP token, got: %s", content)
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

	mgr := New(reg, filepath.Join(t.TempDir(), "does-not-exist-binary"), t.TempDir(), t.TempDir())

	_, _, err := mgr.Launch(context.Background(), "proj-2", LaunchOptions{})
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

	mgr := New(reg, fixtureBin, t.TempDir(), t.TempDir())
	pi, _, err := mgr.Launch(context.Background(), "proj-3", LaunchOptions{})
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

// --- PISO-9: per-project launch dedup ---

func TestLaunch_SecondCallForSameProjectReusesInstance(t *testing.T) {
	fixtureBin := buildFixture(t)
	reg := openTestRegistry(t)
	if err := reg.CreateProject(&registry.Project{ID: "proj-dedup", RootDir: t.TempDir(), Status: "active"}); err != nil {
		t.Fatalf("create project: %v", err)
	}

	mgr := New(reg, fixtureBin, t.TempDir(), t.TempDir())
	mgr.ConnectTimeout = 3 * time.Second

	first, reused, err := mgr.Launch(context.Background(), "proj-dedup", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("first launch: %v", err)
	}
	if reused {
		t.Fatalf("expected the first launch for a project to spawn fresh, got reused=true")
	}
	defer mgr.Close(first.ID)

	second, reused, err := mgr.Launch(context.Background(), "proj-dedup", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("second launch: %v", err)
	}
	if !reused {
		t.Fatalf("expected the second launch for the same live project to be reused")
	}
	if second.ID != first.ID || second.PID != first.PID {
		t.Fatalf("expected the same instance back, got first=%+v second=%+v", first, second)
	}

	// Assert on the real count, not just the response -- the actual bug
	// this test exists to catch is a second process getting spawned even
	// if the response happened to look plausible.
	rows, err := reg.ListProcessInstancesByProject("proj-dedup")
	if err != nil {
		t.Fatalf("list by project: %v", err)
	}
	readyCount := 0
	for _, r := range rows {
		if r.Status == registry.StatusReady {
			readyCount++
		}
	}
	if readyCount != 1 {
		t.Fatalf("expected exactly 1 ready instance row for the project, got %d: %+v", readyCount, rows)
	}
}

func TestLaunch_DifferentProjectStillSpawns(t *testing.T) {
	fixtureBin := buildFixture(t)
	reg := openTestRegistry(t)
	for _, id := range []string{"proj-x", "proj-y"} {
		if err := reg.CreateProject(&registry.Project{ID: id, RootDir: t.TempDir(), Status: "active"}); err != nil {
			t.Fatalf("create project %s: %v", id, err)
		}
	}

	mgr := New(reg, fixtureBin, t.TempDir(), t.TempDir())
	mgr.ConnectTimeout = 3 * time.Second

	piX, reusedX, err := mgr.Launch(context.Background(), "proj-x", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("launch proj-x: %v", err)
	}
	defer mgr.Close(piX.ID)
	if reusedX {
		t.Fatalf("proj-x has no prior instance, must not be reused")
	}

	piY, reusedY, err := mgr.Launch(context.Background(), "proj-y", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("launch proj-y: %v", err)
	}
	defer mgr.Close(piY.ID)
	if reusedY {
		t.Fatalf("proj-y has no prior instance, must not be reused")
	}

	if piX.ID == piY.ID || piX.PID == piY.PID {
		t.Fatalf("expected two distinct instances for two distinct projects, got %+v and %+v", piX, piY)
	}
}

func TestLaunch_DeadRegisteredInstanceStillRelaunches(t *testing.T) {
	fixtureBin := buildFixture(t)
	reg := openTestRegistry(t)
	if err := reg.CreateProject(&registry.Project{ID: "proj-dead", RootDir: t.TempDir(), Status: "active"}); err != nil {
		t.Fatalf("create project: %v", err)
	}

	mgr := New(reg, fixtureBin, t.TempDir(), t.TempDir())
	mgr.ConnectTimeout = 3 * time.Second

	first, _, err := mgr.Launch(context.Background(), "proj-dead", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("first launch: %v", err)
	}

	// Simulate a real crash -- kill the OS process directly, leaving the
	// registry row still saying "ready" (mgr.Close, which also updates the
	// row, is deliberately not used here).
	proc, err := os.FindProcess(first.PID)
	if err != nil {
		t.Fatalf("find process: %v", err)
	}
	if err := proc.Kill(); err != nil {
		t.Fatalf("kill process: %v", err)
	}
	_, _ = proc.Wait()

	// Give health.SocketResponsive something unambiguous to fail against:
	// poll until the socket genuinely stops accepting connections instead
	// of racing the kill.
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if !socketStillResponds(first.SocketPath) {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	second, reused, err := mgr.Launch(context.Background(), "proj-dead", LaunchOptions{Headless: true})
	if err != nil {
		t.Fatalf("relaunch after crash: %v", err)
	}
	defer mgr.Close(second.ID)
	if reused {
		t.Fatalf("a dead-but-registered instance must not be reused, got reused=true for %+v", second)
	}
	if second.ID == first.ID {
		t.Fatalf("expected a genuinely new instance after the crash, got the same id back")
	}

	row, err := reg.GetProcessInstance(first.ID)
	if err != nil {
		t.Fatalf("get original instance row: %v", err)
	}
	if row.Status != registry.StatusCrashed {
		t.Fatalf("expected the dead instance's row to be marked crashed, got %s", row.Status)
	}
}

func TestLaunch_ConcurrentDoubleLaunchYieldsOneProcess(t *testing.T) {
	fixtureBin := buildFixture(t)
	reg := openTestRegistry(t)
	if err := reg.CreateProject(&registry.Project{ID: "proj-concurrent", RootDir: t.TempDir(), Status: "active"}); err != nil {
		t.Fatalf("create project: %v", err)
	}

	mgr := New(reg, fixtureBin, t.TempDir(), t.TempDir())
	mgr.ConnectTimeout = 3 * time.Second

	const n = 8
	type result struct {
		pi     registry.ProcessInstance
		reused bool
		err    error
	}
	results := make(chan result, n)
	var wg sync.WaitGroup
	wg.Add(n)
	for i := 0; i < n; i++ {
		go func() {
			defer wg.Done()
			pi, reused, err := mgr.Launch(context.Background(), "proj-concurrent", LaunchOptions{Headless: true})
			results <- result{pi: pi, reused: reused, err: err}
		}()
	}
	wg.Wait()
	close(results)

	var spawned, reused int
	var ids = map[string]bool{}
	for r := range results {
		if r.err != nil {
			t.Fatalf("concurrent launch: %v", r.err)
		}
		ids[r.pi.ID] = true
		if r.reused {
			reused++
		} else {
			spawned++
		}
	}
	if spawned != 1 {
		t.Fatalf("expected exactly 1 of %d concurrent launches to actually spawn, got %d", n, spawned)
	}
	if reused != n-1 {
		t.Fatalf("expected the other %d launches to be reused, got %d", n-1, reused)
	}
	if len(ids) != 1 {
		t.Fatalf("expected all %d concurrent launches to converge on one instance id, got %d distinct: %v", n, len(ids), ids)
	}

	rows, err := reg.ListProcessInstancesByProject("proj-concurrent")
	if err != nil {
		t.Fatalf("list by project: %v", err)
	}
	readyCount := 0
	for _, row := range rows {
		if row.Status == registry.StatusReady {
			readyCount++
		}
	}
	if readyCount != 1 {
		t.Fatalf("expected exactly 1 ready instance row after %d concurrent launches, got %d: %+v", n, readyCount, rows)
	}
}

// socketStillResponds is a thin local wrapper so the dead-instance test
// above doesn't need to import internal/health directly just for one poll
// loop.
func socketStillResponds(path string) bool {
	c, err := transport.DialTimeout(path, 50*time.Millisecond)
	if err != nil {
		return false
	}
	_ = c.Close()
	return true
}
