package daemon

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"testing"
	"time"

	"snapshotd/internal/config"
	"snapshotd/internal/health"
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
	subscription := call("daemon.subscribeProjects", nil)
	if !bytes.Contains(subscription, []byte(`"mode":"poll"`)) {
		t.Fatalf("expected polling project subscription payload: %s", subscription)
	}

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

func TestDaemon_ExternalRegistrationAndMcpContextIsolation(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	projectA, err := d.CreateProject(ctx, CreateProjectParams{Name: "project-a"})
	if err != nil {
		t.Fatalf("create project A: %v", err)
	}
	projectB, err := d.CreateProject(ctx, CreateProjectParams{Name: "project-b"})
	if err != nil {
		t.Fatalf("create project B: %v", err)
	}

	params := RegisterExternalInstanceParams{
		InstanceNonce: "nonce-a",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   filepath.Join(projectA.RootDir, projectA.MltFileName),
	}
	first, err := d.RegisterExternalInstance(ctx, params)
	if err != nil {
		t.Fatalf("register external instance: %v", err)
	}
	second, err := d.RegisterExternalInstance(ctx, params)
	if err != nil || second.Instance.ID != first.Instance.ID {
		t.Fatalf("registration must be idempotent: first=%+v second=%+v err=%v", first, second, err)
	}
	updated, err := d.UpdateOpenProject(ctx, UpdateExternalProjectParams{
		InstanceID:  first.Instance.ID,
		ProjectPath: filepath.Join(projectB.RootDir, projectB.MltFileName),
		Reason:      "switched",
		Generation:  2,
	})
	if err != nil || updated.ProjectPath != filepath.Join(projectB.RootDir, projectB.MltFileName) {
		t.Fatalf("update project: %+v err=%v", updated, err)
	}
	if _, err := d.HeartbeatExternalInstance(ctx, first.Instance.ID); err != nil {
		t.Fatalf("heartbeat: %v", err)
	}

	owner, err := d.RegisterMcpContext(ctx, RegisterMcpContextParams{
		ContextToken:           "token-a",
		ACPSessionID:           "acp-a",
		ChatProjectID:          projectA.ID,
		DefaultTargetProjectID: projectA.ID,
	})
	if err != nil {
		t.Fatalf("register MCP context: %v", err)
	}
	target, err := d.SetMcpProjectTarget(ctx, owner.ContextToken, projectB.ID)
	if err != nil || target.ChatProjectID != projectA.ID || target.TargetProjectID != projectB.ID {
		t.Fatalf("MCP target must be mutable without moving chat ownership: %+v err=%v", target, err)
	}
	if err := d.UnregisterMcpContext(ctx, owner.ContextToken); err != nil {
		t.Fatalf("unregister MCP context: %v", err)
	}
	if _, err := d.Reg.GetMcpContext(owner.ContextToken); !errors.Is(err, registry.ErrNotFound) {
		t.Fatalf("expected MCP context to be deleted, err=%v", err)
	}
	if err := d.UnregisterMcpContext(ctx, owner.ContextToken); err != nil {
		t.Fatalf("unregister MCP context must be idempotent: %v", err)
	}
	if err := d.UnregisterExternalInstance(ctx, first.Instance.ID); err != nil {
		t.Fatalf("unregister external instance: %v", err)
	}
	closed, err := d.Reg.GetExternalInstance(first.Instance.ID)
	if err != nil || closed.Status != registry.ExternalStatusClosed {
		t.Fatalf("expected closed external instance: %+v err=%v", closed, err)
	}
}

func TestDaemon_ReconcileExternalLeaseAndProjectAggregate(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "aggregate"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	registered, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "aggregate-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   filepath.Join(project.RootDir, project.MltFileName),
	})
	if err != nil {
		t.Fatalf("register: %v", err)
	}
	contextRecord, err := d.RegisterMcpContext(ctx, RegisterMcpContextParams{
		ContextToken:           "expired-context",
		ACPSessionID:           "acp-expired",
		ChatProjectID:          project.ID,
		DefaultTargetProjectID: project.ID,
	})
	if err != nil {
		t.Fatalf("register MCP context: %v", err)
	}
	contextRecord.LeaseExpiresAt = time.Now().UTC().Add(-time.Second)
	if err := d.Reg.SaveMcpContext(&contextRecord); err != nil {
		t.Fatalf("expire MCP context: %v", err)
	}
	projects, err := d.ListProjects(ctx)
	if err != nil || len(projects) != 1 || !projects[0].Open || projects[0].InstanceCount != 1 {
		t.Fatalf("expected registered project aggregate, projects=%+v err=%v", projects, err)
	}
	row, err := d.Reg.GetExternalInstance(registered.Instance.ID)
	if err != nil {
		t.Fatalf("get external instance: %v", err)
	}
	row.LeaseExpiresAt = time.Now().UTC().Add(-time.Second)
	if err := d.Reg.SaveExternalInstance(row); err != nil {
		t.Fatalf("expire lease: %v", err)
	}
	if _, err := d.ReconcileExternalInstances(ctx); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	row, err = d.Reg.GetExternalInstance(registered.Instance.ID)
	if err != nil || row.Status != registry.ExternalStatusStale {
		t.Fatalf("expected stale lease: %+v err=%v", row, err)
	}
	if _, err := d.Reg.GetMcpContext("expired-context"); !errors.Is(err, registry.ErrNotFound) {
		t.Fatalf("expected expired MCP context to be removed, err=%v", err)
	}
}

func TestDaemon_DiscoveryPromotesVerifiedCandidate(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	apps := filepath.Join(d.Cfg.HomeDir, "apps")
	if err := os.MkdirAll(apps, 0o700); err != nil {
		t.Fatal(err)
	}
	endpoint := filepath.Join(apps, "discovery.sock")
	listener, err := net.Listen("unix", endpoint)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	processStart := mustProcessStart(t)
	nonce := "discovery-promote"
	projectDir := filepath.Join(t.TempDir(), "project")
	if err := os.MkdirAll(projectDir, 0o755); err != nil {
		t.Fatal(err)
	}
	projectPath := filepath.Join(projectDir, registry.DefaultMltFileName)
	if err := os.WriteFile(projectPath, []byte("<mlt/>"), 0o644); err != nil {
		t.Fatal(err)
	}
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		var request struct {
			Params struct {
				Challenge string `json:"challenge"`
			} `json:"params"`
		}
		if json.NewDecoder(bufio.NewReader(conn)).Decode(&request) != nil {
			return
		}
		_ = json.NewEncoder(conn).Encode(map[string]any{"result": map[string]any{
			"instanceNonce": nonce,
			"pid":           os.Getpid(),
			"processStart":  processStart,
			"projectPath":   projectPath,
			"challenge":     request.Params.Challenge,
		}})
	}()
	descriptor, err := json.Marshal(map[string]any{
		"endpoint":        endpoint,
		"pid":             os.Getpid(),
		"processStart":    processStart,
		"instanceNonce":   nonce,
		"protocolVersion": 1,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(apps, "discovery.json"), descriptor, 0o600); err != nil {
		t.Fatal(err)
	}
	candidates, err := d.DiscoverExternalInstances(ctx)
	if err != nil || len(candidates) != 1 || !candidates[0].Verified {
		t.Fatalf("expected verified discovery candidate: candidates=%+v err=%v", candidates, err)
	}
	registered, err := d.Reg.GetExternalInstanceByNonce(nonce)
	if err != nil || registered.Status != registry.ExternalStatusOpen {
		t.Fatalf("expected verified candidate to be registered open: %+v err=%v", registered, err)
	}
}

func mustProcessStart(t *testing.T) string {
	t.Helper()
	start, err := health.ProcessStartIdentity(os.Getpid())
	if err != nil {
		t.Fatalf("process start identity: %v", err)
	}
	return start
}

func boolPtr(b bool) *bool { return &b }

// TestDaemon_StopRequest covers the cross-platform stop path added to
// replace `snapshotd stop` signaling a PID directly: (*os.Process).Signal
// doesn't support SIGTERM on Windows, so daemon.stop/RequestStop/
// StopRequested is the only mechanism `snapshotd stop` can rely on there.
func TestDaemon_StopRequest(t *testing.T) {
	fixtureBin := buildFixture(t)
	d := newTestDaemon(t, fixtureBin)
	ctx := context.Background()

	select {
	case <-d.StopRequested():
		t.Fatalf("StopRequested channel should not be closed before RequestStop/daemon.stop")
	default:
	}

	result, err := d.Dispatch(ctx, "daemon.stop", nil)
	if err != nil {
		t.Fatalf("dispatch daemon.stop: %v", err)
	}
	if result == nil {
		t.Fatalf("expected a non-nil result from daemon.stop")
	}

	select {
	case <-d.StopRequested():
	default:
		t.Fatalf("expected StopRequested channel to be closed after daemon.stop")
	}

	// A second call must not panic (sync.Once-guarded close).
	if _, err := d.Dispatch(ctx, "daemon.stop", nil); err != nil {
		t.Fatalf("second dispatch daemon.stop: %v", err)
	}
}
