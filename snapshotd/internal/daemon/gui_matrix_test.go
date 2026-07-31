package daemon

// GUI discovery matrix edge cases from
// memory/snapshotd/gen/plans/native-rust-client-project-discovery/00-plan.md
// "Review and runtime matrix". These drive the real daemon control plane the
// way panel-rust's SnapshotdControlClient does (register / update / heartbeat /
// MCP context / discovery), without requiring a full Shotcut+VNC host.

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"

	"snapshotd/internal/health"
	"snapshotd/internal/registry"
)

// matrix scenarios covered here (plan table rows):
//  1. Normal GUI start with daemon available
//  2. GUI starts before daemon (discovery endpoint survives → later register)
//  3. Daemon restart (re-register same nonce → one live instance)
//  4. Open / Save As / switch / close lifecycle updates
//  5. A-owned chat targets Project B through MCP (chat owner immutable)
//  6. App crash → lease/PID reconciliation marks stale, not open
//  7. Legacy GUI / no discovery endpoint → never falsely open

func TestGUIMatrix_DaemonAvailableAtGUIStart(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "gui-start"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	reg, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "gui-start-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: filepath.Join(t.TempDir(), "gui-start.sap.sock"),
	})
	if err != nil {
		t.Fatalf("register: %v", err)
	}
	if reg.Instance.Status != registry.ExternalStatusOpen {
		t.Fatalf("expected open registration: %+v", reg.Instance)
	}
	if _, err := d.HeartbeatExternalInstance(ctx, reg.Instance.ID); err != nil {
		t.Fatalf("heartbeat: %v", err)
	}
	listed, err := d.ListProjects(ctx)
	if err != nil || len(listed) != 1 || !listed[0].Open {
		t.Fatalf("daemon-available start must show project open: %+v err=%v", listed, err)
	}
	if listed[0].DiscoveryState != "registered" {
		t.Fatalf("expected discoveryState=registered, got %q", listed[0].DiscoveryState)
	}
}

func TestGUIMatrix_GUIRegistrationPreventsHeadlessDuplicateOnProjectOpen(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "gui-reuse"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	if _, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "gui-reuse-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: filepath.Join(t.TempDir(), "not-a-real-gui.sap.sock"),
	}); err != nil {
		t.Fatalf("register GUI: %v", err)
	}

	// The fake GUI socket makes the final SAP bind fail, but the important
	// invariant is checked before that bind: project.open must not launch a
	// daemon-owned headless process while the GUI registration is live.
	_, _ = d.ForwardSAP(ctx, "gui-reuse-session", &fanoutSink{}, "project.enter", mustJSON(t, map[string]any{
		"projectId": project.ID,
	}))
	instances, err := d.Reg.ListProcessInstancesByProject(project.ID)
	if err != nil {
		t.Fatalf("list process instances: %v", err)
	}
	if len(instances) != 0 {
		t.Fatalf("GUI-owned project.open launched a duplicate daemon process: %+v", instances)
	}
}

func TestGUIMatrix_MCPOpenPromotesDiscoveryBeforeHeadlessLaunch(t *testing.T) {
	// Cold-start race: panel-rust has published its discovery descriptor, but
	// its asynchronous control registration has not reached snapshotd yet.
	// project.open must promote that descriptor before Proc.Launch is allowed.
	d := newTestDaemon(t, buildFixture(t))
	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()
	projectDir := filepath.Join(t.TempDir(), "cold-start-project")
	if err := os.MkdirAll(projectDir, 0o755); err != nil {
		t.Fatal(err)
	}
	projectPath := filepath.Join(projectDir, registry.DefaultMltFileName)
	if err := os.WriteFile(projectPath, []byte("<mlt/>"), 0o644); err != nil {
		t.Fatal(err)
	}
	project, err := d.resolveOrRegisterProjectByPath(projectPath)
	if err != nil {
		t.Fatal(err)
	}

	apps := filepath.Join(d.Cfg.HomeDir, "apps")
	if err := os.MkdirAll(apps, 0o700); err != nil {
		t.Fatal(err)
	}
	discoverySocket := "/tmp/snapflow-cold-discovery.sock"
	_ = os.Remove(discoverySocket)
	discoveryListener, err := net.Listen("unix", discoverySocket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = discoveryListener.Close(); _ = os.Remove(discoverySocket) })
	sapSocket := "/tmp/snapflow-cold-start.sap.sock"
	_ = os.Remove(sapSocket)
	sapListener, err := net.Listen("unix", sapSocket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = sapListener.Close(); _ = os.Remove(sapSocket) })
	go answerDiscoverOnceWithSAP(t, discoveryListener, "cold-start-nonce", mustProcessStart(t), projectPath, sapSocket)
	descriptor, _ := json.Marshal(map[string]any{
		"endpoint": discoverySocket, "pid": os.Getpid(), "processStart": mustProcessStart(t),
		"instanceNonce": "cold-start-nonce", "protocolVersion": 1,
	})
	if err := os.WriteFile(filepath.Join(apps, "cold-start.json"), descriptor, 0o600); err != nil {
		t.Fatal(err)
	}

	// The fake SAP listener intentionally does not complete project.select;
	// the invariant under test is checked before SAP bind completes.
	_, _ = d.ForwardSAP(ctx, "cold-start-mcp", &fanoutSink{}, "project.enter", mustJSON(t, map[string]any{
		"projectId": project.ID,
	}))
	instances, err := d.Reg.ListProcessInstancesByProject(project.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(instances) != 0 {
		t.Fatalf("cold-start MCP open launched a headless duplicate: %+v", instances)
	}
	external, err := d.Reg.GetExternalInstanceByNonce("cold-start-nonce")
	if err != nil {
		t.Fatalf("discovery was not promoted: %v", err)
	}
	if external.SAPSocketPath != sapSocket || external.ProjectPath != projectPath {
		t.Fatalf("promoted discovery lost GUI ownership data: %+v", external)
	}
}

func TestGUIMatrix_MCPWaitsForGuiSAPAfterDiscovery(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "sap-ready-race"})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	sapSocket := "/tmp/snapflow-sap-ready-race.sock"
	_ = os.Remove(sapSocket)
	if _, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "sap-ready-race-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: sapSocket,
	}); err != nil {
		t.Fatal(err)
	}

	result := make(chan error, 1)
	go func() {
		_, err := d.awaitLiveExternalProject(ctx, project.ID)
		result <- err
	}()
	time.Sleep(150 * time.Millisecond)
	listener, err := net.Listen("unix", sapSocket)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		_ = listener.Close()
		_ = os.Remove(sapSocket)
	}()
	select {
	case err := <-result:
		if err != nil {
			t.Fatalf("awaitLiveExternalProject returned early: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("awaitLiveExternalProject did not wait for SAP readiness")
	}
}

func TestGUIMatrix_DaemonFirstGUIRegistrationDrainsHeadlessBeforeLease(t *testing.T) {
	d := newSapFixtureDaemon(t, buildSapFixture(t))
	d.Proc.RunDir = filepath.Join("/tmp", "gui-matrix-handoff-run")
	if err := os.MkdirAll(d.Proc.RunDir, 0o755); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "daemon-first-handoff"})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	guiSocket := filepath.Join(t.TempDir(), "gui.sap.sock")
	guiListener, err := net.Listen("unix", guiSocket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = guiListener.Close() })
	if !health.SocketResponsive(guiSocket, time.Second) {
		t.Fatalf("GUI socket test listener was not responsive: %s", guiSocket)
	}
	launched, err := d.Launch(ctx, LaunchParams{ProjectID: project.ID})
	if err != nil {
		t.Fatalf("daemon launch: %v", err)
	}
	registered, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "daemon-first-handoff-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: guiSocket,
	})
	if err != nil {
		t.Fatalf("GUI registration/handoff: %v", err)
	}
	if registered.Instance.Status != registry.ExternalStatusOpen {
		t.Fatalf("GUI lease not open: %+v", registered.Instance)
	}
	row, err := d.Reg.GetProcessInstance(launched.ID)
	if err != nil {
		t.Fatal(err)
	}
	if row.Status == registry.StatusReady || health.PIDAlive(row.PID) {
		t.Fatalf("daemon-first handoff left headless process alive: %+v", row)
	}
	instances, err := d.Reg.ListProcessInstancesByProject(project.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, instance := range instances {
		if instance.Status == registry.StatusStarting || instance.Status == registry.StatusReady {
			t.Fatalf("handoff exposed a live daemon instance: %+v", instances)
		}
	}
}

func TestGUIMatrix_LateGuiSocketUpdateCompletesHandoff(t *testing.T) {
	d := newSapFixtureDaemon(t, buildSapFixture(t))
	d.Proc.RunDir = filepath.Join("/tmp", "gui-matrix-late-socket-run")
	if err := os.MkdirAll(d.Proc.RunDir, 0o755); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "late-gui-socket"})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	launched, err := d.Launch(ctx, LaunchParams{ProjectID: project.ID})
	if err != nil {
		t.Fatal(err)
	}
	guiSocket := filepath.Join(t.TempDir(), "late-gui.sap.sock")
	registered, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "late-gui-socket-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: guiSocket,
	})
	if err != nil {
		t.Fatal(err)
	}
	guiListener, err := net.Listen("unix", guiSocket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = guiListener.Close() })
	if _, err := d.UpdateOpenProject(ctx, UpdateExternalProjectParams{
		InstanceID:  registered.Instance.ID,
		ProjectPath: path,
		Reason:      "opened",
		Generation:  1,
	}); err != nil {
		t.Fatalf("late GUI update: %v", err)
	}
	row, err := d.Reg.GetProcessInstance(launched.ID)
	if err != nil {
		t.Fatal(err)
	}
	if row.Status == registry.StatusReady || health.PIDAlive(row.PID) {
		t.Fatalf("late GUI socket update left daemon process alive: %+v", row)
	}
}

func TestGUIMatrix_McpCloseDoesNotKillGuiOwnerThenRetryAfterGuiClose(t *testing.T) {
	d := newSapFixtureDaemon(t, buildSapFixture(t))
	d.Proc.RunDir = filepath.Join("/tmp", "gui-matrix-close-run")
	if err := os.MkdirAll(d.Proc.RunDir, 0o755); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "close-order"})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	registered, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "close-order-gui-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: filepath.Join(t.TempDir(), "gui.sap.sock"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := d.RegisterMcpContext(ctx, RegisterMcpContextParams{
		ContextToken:  "close-order-mcp",
		ACPSessionID:  "close-order-session",
		ChatProjectID: project.ID,
	}); err != nil {
		t.Fatal(err)
	}
	if err := d.UnregisterMcpContext(ctx, "close-order-mcp"); err != nil {
		t.Fatal(err)
	}
	stillGui, err := d.Reg.GetExternalInstance(registered.Instance.ID)
	if err != nil || stillGui.Status != registry.ExternalStatusOpen {
		t.Fatalf("MCP close killed GUI lease: %+v err=%v", stillGui, err)
	}

	if err := d.UnregisterExternalInstance(ctx, registered.Instance.ID); err != nil {
		t.Fatal(err)
	}
	launched, err := d.Launch(ctx, LaunchParams{ProjectID: project.ID})
	if err != nil {
		t.Fatalf("headless retry after GUI close: %v", err)
	}
	instances, err := d.Reg.ListProcessInstancesByProject(project.ID)
	if err != nil {
		t.Fatal(err)
	}
	ready := 0
	for _, instance := range instances {
		if instance.Status == registry.StatusReady {
			ready++
		}
	}
	if ready != 1 || launched.Status != registry.StatusReady {
		t.Fatalf("explicit retry did not create exactly one headless instance: %+v", instances)
	}
}

func TestGUIMatrix_GUIStartsBeforeDaemon_DiscoveryPromotesOnce(t *testing.T) {
	// Simulates: panel publishes discovery descriptor+endpoint while the
	// daemon is down; later DiscoverExternalInstances promotes once.
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	apps := filepath.Join(d.Cfg.HomeDir, "apps")
	if err := os.MkdirAll(apps, 0o700); err != nil {
		t.Fatal(err)
	}
	projectDir := filepath.Join(t.TempDir(), "late-daemon-project")
	if err := os.MkdirAll(projectDir, 0o755); err != nil {
		t.Fatal(err)
	}
	projectPath := filepath.Join(projectDir, registry.DefaultMltFileName)
	if err := os.WriteFile(projectPath, []byte("<mlt/>"), 0o644); err != nil {
		t.Fatal(err)
	}
	endpoint := filepath.Join(apps, "late-daemon.sock")
	listener, err := net.Listen("unix", endpoint)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	processStart := mustProcessStart(t)
	nonce := "late-daemon-nonce"
	go answerDiscoverOnce(t, listener, nonce, processStart, projectPath)

	descriptor, _ := json.Marshal(map[string]any{
		"endpoint": endpoint, "pid": os.Getpid(), "processStart": processStart,
		"instanceNonce": nonce, "protocolVersion": 1,
	})
	if err := os.WriteFile(filepath.Join(apps, "late-daemon.json"), descriptor, 0o600); err != nil {
		t.Fatal(err)
	}

	// First discovery: promote.
	c1, err := d.DiscoverExternalInstances(ctx)
	if err != nil || len(c1) != 1 || !c1[0].Verified {
		t.Fatalf("first discovery: %+v err=%v", c1, err)
	}
	first, err := d.Reg.GetExternalInstanceByNonce(nonce)
	if err != nil {
		t.Fatalf("first register: %v", err)
	}
	// Second discovery: must not duplicate (idempotent by nonce).
	// Endpoint only answers once; a second ping may fail verification, but
	// the existing registration must remain a single open row.
	_, _ = d.DiscoverExternalInstances(ctx)
	all, err := d.Reg.ListExternalInstances()
	if err != nil {
		t.Fatal(err)
	}
	openCount := 0
	for _, in := range all {
		if in.InstanceNonce == nonce && in.Status == registry.ExternalStatusOpen {
			openCount++
		}
	}
	if openCount != 1 {
		t.Fatalf("GUI-before-daemon must register exactly once, openCount=%d all=%+v first=%+v", openCount, all, first)
	}
}

func TestGUIMatrix_DaemonRestart_ReRegisterNoDuplicate(t *testing.T) {
	// Same process nonce re-registers after "daemon restart" (fresh daemon
	// state is simulated by unregister+register and by idempotent re-register).
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "restart-proj"})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(project.RootDir, project.MltFileName)
	params := RegisterExternalInstanceParams{
		InstanceNonce: "restart-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   path,
		SAPSocketPath: filepath.Join(t.TempDir(), "restart.sap.sock"),
	}
	first, err := d.RegisterExternalInstance(ctx, params)
	if err != nil {
		t.Fatalf("first: %v", err)
	}
	// Simulated client reconnect after daemon socket comes back: same nonce.
	second, err := d.RegisterExternalInstance(ctx, params)
	if err != nil {
		t.Fatalf("second: %v", err)
	}
	if first.Instance.ID != second.Instance.ID {
		t.Fatalf("re-register must reuse instance id: first=%s second=%s", first.Instance.ID, second.Instance.ID)
	}
	all, err := d.Reg.ListExternalInstances()
	if err != nil {
		t.Fatal(err)
	}
	if len(all) != 1 {
		t.Fatalf("expected single external instance after reconnect, got %d: %+v", len(all), all)
	}
	listed, err := d.ListProjects(ctx)
	if err != nil || len(listed) != 1 || listed[0].InstanceCount < 1 || !listed[0].Open {
		t.Fatalf("after reconnect list must show one open project: %+v err=%v", listed, err)
	}
}

func TestGUIMatrix_OpenSaveAsSwitchClose(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	a, err := d.CreateProject(ctx, CreateProjectParams{Name: "life-a"})
	if err != nil {
		t.Fatal(err)
	}
	b, err := d.CreateProject(ctx, CreateProjectParams{Name: "life-b"})
	if err != nil {
		t.Fatal(err)
	}
	pathA := filepath.Join(a.RootDir, a.MltFileName)
	pathB := filepath.Join(b.RootDir, b.MltFileName)
	reg, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "life-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   pathA,
	})
	if err != nil {
		t.Fatal(err)
	}
	id := reg.Instance.ID

	// opened
	opened, err := d.UpdateOpenProject(ctx, UpdateExternalProjectParams{
		InstanceID: id, ProjectPath: pathA, Reason: "opened", Generation: 1,
	})
	if err != nil || opened.LifecycleReason != "opened" || opened.Generation != 1 {
		t.Fatalf("opened: %+v err=%v", opened, err)
	}

	// Save As A → B (path change with saved_as reason)
	savedAs, err := d.UpdateOpenProject(ctx, UpdateExternalProjectParams{
		InstanceID: id, ProjectPath: pathB, Reason: "saved_as", Generation: 2,
	})
	if err != nil || savedAs.ProjectPath != pathB || savedAs.LifecycleReason != "saved_as" {
		t.Fatalf("saved_as: %+v err=%v", savedAs, err)
	}

	// switch back to A
	switched, err := d.UpdateOpenProject(ctx, UpdateExternalProjectParams{
		InstanceID: id, ProjectPath: pathA, Reason: "switched", Generation: 3,
	})
	if err != nil || switched.ProjectPath != pathA || switched.Generation != 3 {
		t.Fatalf("switched: %+v err=%v", switched, err)
	}

	// close → no project path; unregister marks closed
	closedUpdate, err := d.UpdateOpenProject(ctx, UpdateExternalProjectParams{
		InstanceID: id, ProjectPath: "", Reason: "closed", Generation: 4,
	})
	if err != nil || closedUpdate.ProjectPath != "" {
		t.Fatalf("closed update: %+v err=%v", closedUpdate, err)
	}
	if err := d.UnregisterExternalInstance(ctx, id); err != nil {
		t.Fatalf("unregister: %v", err)
	}
	row, err := d.Reg.GetExternalInstance(id)
	if err != nil || row.Status != registry.ExternalStatusClosed {
		t.Fatalf("expected closed instance: %+v err=%v", row, err)
	}
	// Closed external must not keep either project falsely open (unless a
	// ready ProcessInstance remains — none here).
	listed, err := d.ListProjects(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range listed {
		if p.Open && !p.Active {
			// Open only from external lease; Active is process-instance.
			t.Fatalf("closed GUI must not leave project open: %+v", p)
		}
		if p.Open {
			t.Fatalf("closed GUI must not leave project open: %+v", p)
		}
	}
}

func TestGUIMatrix_AChatTargetsBViaMCP_ChatOwnerImmutable(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	a, err := d.CreateProject(ctx, CreateProjectParams{Name: "mcp-a"})
	if err != nil {
		t.Fatal(err)
	}
	b, err := d.CreateProject(ctx, CreateProjectParams{Name: "mcp-b"})
	if err != nil {
		t.Fatal(err)
	}
	ctxRec, err := d.RegisterMcpContext(ctx, RegisterMcpContextParams{
		ContextToken:           "gui-matrix-token",
		ACPSessionID:           "acp-a",
		ChatProjectID:          a.ID,
		DefaultTargetProjectID: a.ID,
	})
	if err != nil {
		t.Fatal(err)
	}
	target, err := d.SetMcpProjectTarget(ctx, ctxRec.ContextToken, b.ID)
	if err != nil {
		t.Fatal(err)
	}
	if target.ChatProjectID != a.ID {
		t.Fatalf("chat ownership must stay A, got %s", target.ChatProjectID)
	}
	if target.TargetProjectID != b.ID {
		t.Fatalf("MCP target must be B, got %s", target.TargetProjectID)
	}
	// Concurrent second agent on B must not steal A's context.
	other, err := d.RegisterMcpContext(ctx, RegisterMcpContextParams{
		ContextToken:           "gui-matrix-token-b",
		ACPSessionID:           "acp-b",
		ChatProjectID:          b.ID,
		DefaultTargetProjectID: b.ID,
	})
	if err != nil {
		t.Fatal(err)
	}
	if other.ChatProjectID != b.ID || other.TargetProjectID != b.ID {
		t.Fatalf("second context isolation failed: %+v", other)
	}
	// A's token still targets B.
	again, err := d.Reg.GetMcpContext("gui-matrix-token")
	if err != nil || again.ChatProjectID != a.ID || again.TargetProjectID != b.ID {
		t.Fatalf("A context must remain chat=A target=B: %+v err=%v", again, err)
	}
}

func TestGUIMatrix_AppCrashLeaseExpiry_NoFalseOpen(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "crash-proj"})
	if err != nil {
		t.Fatal(err)
	}
	reg, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
		InstanceNonce: "crash-nonce",
		PID:           os.Getpid(),
		ProcessStart:  mustProcessStart(t),
		ProjectPath:   filepath.Join(project.RootDir, project.MltFileName),
	})
	if err != nil {
		t.Fatal(err)
	}
	row, err := d.Reg.GetExternalInstance(reg.Instance.ID)
	if err != nil {
		t.Fatal(err)
	}
	// Crash: lease expires without unregister (no graceful close).
	row.LeaseExpiresAt = time.Now().UTC().Add(-2 * time.Second)
	if err := d.Reg.SaveExternalInstance(row); err != nil {
		t.Fatal(err)
	}
	if _, err := d.ReconcileExternalInstances(ctx); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	stale, err := d.Reg.GetExternalInstance(reg.Instance.ID)
	if err != nil || stale.Status != registry.ExternalStatusStale {
		t.Fatalf("expected stale after crash lease: %+v err=%v", stale, err)
	}
	listed, err := d.ListProjects(ctx)
	if err != nil || len(listed) != 1 {
		t.Fatalf("list: %+v err=%v", listed, err)
	}
	if listed[0].Open {
		t.Fatalf("crash/expiry must not leave project open: %+v", listed[0])
	}
	// Resolver must also refuse the expired external socket.
	if _, _, err := d.resolveProjectInstance(project.ID); err == nil {
		t.Fatal("expired external must not resolve for SAP proxy")
	}
}

func TestGUIMatrix_LegacyNoEndpoint_NeverFalselyOpen(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	ctx := context.Background()
	// Known-on-disk project with no discovery descriptor and no registration.
	project, err := d.CreateProject(ctx, CreateProjectParams{Name: "legacy"})
	if err != nil {
		t.Fatal(err)
	}
	// Stale descriptor pointing at a dead socket (legacy/broken endpoint).
	apps := filepath.Join(d.Cfg.HomeDir, "apps")
	if err := os.MkdirAll(apps, 0o700); err != nil {
		t.Fatal(err)
	}
	descriptor, _ := json.Marshal(map[string]any{
		"endpoint": filepath.Join(apps, "missing.sock"),
		"pid":      999999, "processStart": "unix:0",
		"instanceNonce": "legacy-dead", "protocolVersion": 1,
	})
	if err := os.WriteFile(filepath.Join(apps, "legacy.json"), descriptor, 0o600); err != nil {
		t.Fatal(err)
	}
	candidates, err := d.DiscoverExternalInstances(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range candidates {
		if c.Verified {
			t.Fatalf("legacy/missing endpoint must not verify: %+v", c)
		}
	}
	if _, err := d.Reg.GetExternalInstanceByNonce("legacy-dead"); !errors.Is(err, registry.ErrNotFound) {
		t.Fatalf("legacy must not create external row, err=%v", err)
	}
	listed, err := d.ListProjects(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range listed {
		if p.ID == project.ID && (p.Open || p.Active || p.IsOpen) {
			t.Fatalf("legacy GUI must never be shown open: %+v", p)
		}
		if p.DiscoveryState != "known" && p.ID == project.ID {
			t.Fatalf("expected known-only discoveryState, got %+v", p)
		}
	}
}

func answerDiscoverOnce(t *testing.T, listener net.Listener, nonce, processStart, projectPath string) {
	answerDiscoverOnceWithSAP(t, listener, nonce, processStart, projectPath, "")
}

func answerDiscoverOnceWithSAP(t *testing.T, listener net.Listener, nonce, processStart, projectPath, sapSocket string) {
	t.Helper()
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
		"sapSocketPath": sapSocket,
		"challenge":     request.Params.Challenge,
	}})
}
