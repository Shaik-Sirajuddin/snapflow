package registry

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

func openTestRegistry(t *testing.T) *Registry {
	t.Helper()
	dir := t.TempDir()
	reg, err := Open(filepath.Join(dir, "registry.db"))
	if err != nil {
		t.Fatalf("open registry: %v", err)
	}
	t.Cleanup(func() { _ = reg.Close() })
	return reg
}

func seedProjectAndInstance(t *testing.T, reg *Registry, pid int, socketPath string) (Project, ProcessInstance) {
	t.Helper()
	p := Project{ID: "proj-1", RootDir: t.TempDir(), MltFileName: DefaultMltFileName, Status: "active"}
	if err := reg.CreateProject(&p); err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi := ProcessInstance{
		ID:         "inst-1",
		ProjectID:  p.ID,
		PID:        pid,
		SocketPath: socketPath,
		Status:     StatusReady,
	}
	if err := reg.CreateProcessInstance(&pi); err != nil {
		t.Fatalf("create process instance: %v", err)
	}
	return p, pi
}

// spawnSleeper starts a real short-lived child process so PID-alive checks
// exercise a genuine OS pid, and returns its pid plus a func to reap it.
func spawnSleeper(t *testing.T) (pid int, wait func()) {
	t.Helper()
	cmd := exec.Command("sleep", "30")
	if err := cmd.Start(); err != nil {
		t.Fatalf("spawn sleeper: %v", err)
	}
	return cmd.Process.Pid, func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	}
}

func TestReconcile_PIDAliveAndSocketResponsive_StaysReady(t *testing.T) {
	reg := openTestRegistry(t)

	dir := t.TempDir()
	sockPath := filepath.Join(dir, "proj.sock")
	ln, err := net_listenUnix(sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	go acceptLoop(ln)

	pid, reap := spawnSleeper(t)
	defer reap()

	_, _ = seedProjectAndInstance(t, reg, pid, sockPath)

	rc := &Reconciler{
		Reg:           reg,
		PIDAlive:      func(p int) bool { return p == pid },
		SocketHealthy: realSocketHealthy,
		HealthTimeout: time.Second,
	}

	outcomes, err := rc.Reconcile(context.Background())
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if len(outcomes) != 1 {
		t.Fatalf("expected 1 outcome, got %d", len(outcomes))
	}
	if outcomes[0].Action != "reconnected" {
		t.Fatalf("expected reconnected, got %s (err=%v)", outcomes[0].Action, outcomes[0].Err)
	}

	row, err := reg.GetProcessInstance("inst-1")
	if err != nil {
		t.Fatalf("get instance: %v", err)
	}
	if row.Status != StatusReady {
		t.Fatalf("expected status ready, got %s", row.Status)
	}
}

func TestReconcile_PIDDead_MarkedCrashed(t *testing.T) {
	reg := openTestRegistry(t)

	dir := t.TempDir()
	sockPath := filepath.Join(dir, "proj.sock")
	ln, err := net_listenUnix(sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	go acceptLoop(ln)

	// A pid that is certainly not alive: spawn+wait so it's reaped, or pick a
	// very unlikely-to-exist high pid deterministically.
	deadPID := 999999

	_, _ = seedProjectAndInstance(t, reg, deadPID, sockPath)

	rc := &Reconciler{
		Reg:           reg,
		PIDAlive:      func(p int) bool { return false }, // simulate dead pid
		SocketHealthy: realSocketHealthy,
		HealthTimeout: time.Second,
	}

	outcomes, err := rc.Reconcile(context.Background())
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if len(outcomes) != 1 || outcomes[0].Action != "marked_crashed" {
		t.Fatalf("expected marked_crashed, got %+v", outcomes)
	}

	row, err := reg.GetProcessInstance("inst-1")
	if err != nil {
		t.Fatalf("get instance: %v", err)
	}
	if row.Status != StatusCrashed {
		t.Fatalf("expected status crashed, got %s", row.Status)
	}
}

func TestReconcile_PIDAliveButSocketUnresponsive_MarkedCrashed(t *testing.T) {
	reg := openTestRegistry(t)

	// Deliberately do NOT listen on this path -- socket file doesn't exist.
	sockPath := filepath.Join(t.TempDir(), "nobody-listening.sock")

	pid, reap := spawnSleeper(t)
	defer reap()

	_, _ = seedProjectAndInstance(t, reg, pid, sockPath)

	rc := &Reconciler{
		Reg:           reg,
		PIDAlive:      func(p int) bool { return p == pid },
		SocketHealthy: realSocketHealthy,
		HealthTimeout: 200 * time.Millisecond,
	}

	outcomes, err := rc.Reconcile(context.Background())
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if len(outcomes) != 1 || outcomes[0].Action != "marked_crashed" {
		t.Fatalf("expected marked_crashed, got %+v", outcomes)
	}

	row, err := reg.GetProcessInstance("inst-1")
	if err != nil {
		t.Fatalf("get instance: %v", err)
	}
	if row.Status != StatusCrashed {
		t.Fatalf("expected status crashed, got %s", row.Status)
	}
}

func TestReconcile_WithRelaunch_RelaunchesCrashedRow(t *testing.T) {
	reg := openTestRegistry(t)
	sockPath := filepath.Join(t.TempDir(), "gone.sock")

	_, _ = seedProjectAndInstance(t, reg, 999999, sockPath)

	relaunched := false
	rc := &Reconciler{
		Reg:           reg,
		PIDAlive:      func(p int) bool { return false },
		SocketHealthy: realSocketHealthy,
		HealthTimeout: 200 * time.Millisecond,
		Relaunch: func(ctx context.Context, project Project, prior ProcessInstance) (ProcessInstance, error) {
			relaunched = true
			return ProcessInstance{
				ID:         "inst-2",
				ProjectID:  project.ID,
				PID:        os.Getpid(),
				SocketPath: prior.SocketPath,
				Status:     StatusReady,
			}, nil
		},
	}

	outcomes, err := rc.Reconcile(context.Background())
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if !relaunched {
		t.Fatalf("expected Relaunch to be invoked")
	}
	if len(outcomes) != 1 || outcomes[0].Action != "relaunched" {
		t.Fatalf("expected relaunched outcome, got %+v", outcomes)
	}

	if _, err := reg.GetProcessInstance("inst-2"); err != nil {
		t.Fatalf("expected new instance row to be persisted: %v", err)
	}
}
