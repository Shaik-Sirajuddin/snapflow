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

// buildSapFixture compiles the Content-Length SAP stand-in used for
// project-open-init-close daemon tests (open/init audit/multi-session close).
func buildSapFixture(t *testing.T) string {
	t.Helper()
	name := "sap-fixture"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	out := filepath.Join(t.TempDir(), name)
	cmd := exec.Command("go", "build", "-o", out, "snapshotd/internal/procmgr/testdata/sap_fixture")
	if outBytes, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("building sap fixture: %v\n%s", err, outBytes)
	}
	return out
}

func newSapFixtureDaemon(t *testing.T, binPath string) *Daemon {
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
	d.Proc.ConnectTimeout = 5 * time.Second
	t.Cleanup(func() { _ = d.Close() })
	return d
}

func TestProjectOpen_InitAudit_MultiSessionReuseAndClose(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	// Project with an existing .mlt so the fixture simulates a real load.
	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "open-init"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	mltPath := filepath.Join(proj.RootDir, proj.MltFileName)
	if err := os.WriteFile(mltPath, []byte("<mlt/>"), 0o644); err != nil {
		t.Fatalf("seed mlt: %v", err)
	}

	sinkA := &fanoutSink{}
	sinkB := &fanoutSink{}

	// First open: auto Launch-or-reuse + Bind; expect opened=true and one init audit.
	selA, err := d.ForwardSAP(ctx, "session-a", sinkA, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("project.select A: %v", err)
	}
	var stateA map[string]any
	if err := json.Unmarshal(selA, &stateA); err != nil {
		t.Fatalf("unmarshal select A: %v", err)
	}
	if stateA["opened"] != true {
		t.Fatalf("expected opened=true on first select, got %+v", stateA)
	}
	if stateA["mltExisted"] != true {
		t.Fatalf("expected fixture to see existing mlt, got %+v", stateA)
	}

	events, err := d.Reg.ListAuditEvents(proj.ID)
	if err != nil {
		t.Fatalf("list audit: %v", err)
	}
	initN := 0
	for _, e := range events {
		if e.Kind == registry.AuditInit {
			initN++
		}
	}
	if initN != 1 {
		t.Fatalf("expected exactly 1 init audit after first open, got %d (%+v)", initN, events)
	}

	// Mutate via session A so we can detect a reload (which would drop tracks).
	if _, err := d.ForwardSAP(ctx, "session-a", sinkA, "edit.addTrack", mustJSON(t, map[string]any{"kind": "audio"})); err != nil {
		t.Fatalf("addTrack A: %v", err)
	}

	// Second session open: must reuse process+connection, not reload.
	selB, err := d.ForwardSAP(ctx, "session-b", sinkB, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("project.select B: %v", err)
	}
	var stateB map[string]any
	if err := json.Unmarshal(selB, &stateB); err != nil {
		t.Fatalf("unmarshal select B: %v", err)
	}
	if stateB["opened"] != true {
		t.Fatalf("expected opened=true on second select, got %+v", stateB)
	}
	// selectCount is fixture-only: first select + second select (no new process).
	if sc, ok := stateB["selectCount"].(float64); !ok || sc < 2 {
		t.Fatalf("expected selectCount>=2 on shared process, got %+v", stateB)
	}

	// Still exactly one init audit after second open.
	events, err = d.Reg.ListAuditEvents(proj.ID)
	if err != nil {
		t.Fatalf("list audit 2: %v", err)
	}
	initN = 0
	for _, e := range events {
		if e.Kind == registry.AuditInit {
			initN++
		}
	}
	if initN != 1 {
		t.Fatalf("expected still 1 init audit after second open, got %d", initN)
	}

	// Only one process instance for the project (reuse, no second spawn).
	instances, err := d.Reg.ListProcessInstancesByProject(proj.ID)
	if err != nil {
		t.Fatalf("list instances: %v", err)
	}
	ready := 0
	for _, in := range instances {
		if in.Status == registry.StatusReady {
			ready++
		}
	}
	if ready != 1 {
		t.Fatalf("expected 1 ready process instance after dual open, got %d (%+v)", ready, instances)
	}

	// Tracks still include the post-open mutation (no reload discarded edits).
	tracksRaw, err := d.ForwardSAP(ctx, "session-b", sinkB, "edit.listTracks", mustJSON(t, map[string]any{}))
	if err != nil {
		t.Fatalf("listTracks B: %v", err)
	}
	var tracks []map[string]any
	if err := json.Unmarshal(tracksRaw, &tracks); err != nil {
		t.Fatalf("unmarshal tracks: %v", err)
	}
	// Existing mlt seeds one video track; addTrack added audio => at least 2.
	if len(tracks) < 2 {
		t.Fatalf("expected tracks preserved across second open (no reload), got %+v", tracks)
	}

	// Close session A only; B must still work and process stays ready.
	if _, err := d.ForwardSAP(ctx, "session-a", sinkA, "project.exit", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("project.exit A: %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "session-b", sinkB, "edit.listTracks", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("listTracks B after A close: %v", err)
	}
	instances, err = d.Reg.ListProcessInstancesByProject(proj.ID)
	if err != nil {
		t.Fatalf("list instances after close: %v", err)
	}
	ready = 0
	for _, in := range instances {
		if in.Status == registry.StatusReady {
			ready++
		}
	}
	if ready != 1 {
		t.Fatalf("expected process still ready after one session close, got %d", ready)
	}
}

func TestProjectOpen_BindOnlyWhenNoMlt(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "empty-proj"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	// No project.mlt on disk.

	sink := &fanoutSink{}
	sel, err := d.ForwardSAP(ctx, "s1", sink, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("project.select: %v", err)
	}
	var state map[string]any
	if err := json.Unmarshal(sel, &state); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if state["opened"] != true {
		t.Fatalf("expected opened=true (bind-only success), got %+v", state)
	}
	if state["mltExisted"] != false {
		t.Fatalf("expected mltExisted=false, got %+v", state)
	}
	// Bind-only: no seeded tracks from mlt.
	tracksRaw, err := d.ForwardSAP(ctx, "s1", sink, "edit.listTracks", mustJSON(t, map[string]any{}))
	if err != nil {
		t.Fatalf("listTracks: %v", err)
	}
	var tracks []map[string]any
	if err := json.Unmarshal(tracksRaw, &tracks); err != nil {
		t.Fatalf("unmarshal tracks: %v", err)
	}
	if len(tracks) != 0 {
		t.Fatalf("expected empty tracks for missing mlt bind-only, got %+v", tracks)
	}
}

// TestProjectOpen_RealSapRust_OpenExistingMlt exercises the full C++/Rust
// open path when a real_ffi snapflow binary is available. Skips otherwise.
func TestProjectOpen_RealSapRust_OpenExistingMlt(t *testing.T) {
	binPath := realSapRustBinary(t)

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
	d.Proc.ConnectTimeout = 30 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	// Prefer a real saved project.mlt from ~/.snapshotd if present; else seed a minimal one.
	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "real-open"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	src := os.Getenv("HOME") + "/.snapshotd/projects/odyssey-explainer-trailer/project.mlt"
	if data, err := os.ReadFile(src); err == nil && len(data) > 100 {
		if err := os.WriteFile(filepath.Join(proj.RootDir, proj.MltFileName), data, 0o644); err != nil {
			t.Fatalf("copy mlt: %v", err)
		}
	} else {
		// Minimal MLT XML — may fail open depending on version; still asserts Bind path.
		minimal := `<?xml version="1.0" encoding="utf-8"?>
<mlt LC_NUMERIC="C" version="7.0.0" root="/tmp" title="test">
  <profile description="HD 1080p 25 fps" width="1920" height="1080" progressive="1" sample_aspect_num="1" sample_aspect_den="1" display_aspect_num="16" display_aspect_den="9" frame_rate_num="25" frame_rate_den="1" colorspace="709"/>
  <producer id="black" in="0" out="24">
    <property name="length">25</property>
    <property name="eof">pause</property>
    <property name="resource">0</property>
    <property name="mlt_service">color</property>
  </producer>
  <playlist id="main_bin" title="Shotcut playlist">
    <property name="xml_retain">1</property>
  </playlist>
  <tractor id="tractor0" title="Shotcut Project Tractor" in="0" out="24">
    <property name="shotcut">1</property>
    <track producer="black"/>
  </tractor>
</mlt>
`
		if err := os.WriteFile(filepath.Join(proj.RootDir, proj.MltFileName), []byte(minimal), 0o644); err != nil {
			t.Fatalf("write minimal mlt: %v", err)
		}
	}

	sink := &fanoutSink{}
	sel, err := d.ForwardSAP(ctx, "real-a", sink, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("project.select real open: %v", err)
	}
	var state map[string]any
	if err := json.Unmarshal(sel, &state); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if state["opened"] != true {
		t.Fatalf("expected opened=true after real open, got %+v", state)
	}

	events, err := d.Reg.ListAuditEvents(proj.ID)
	if err != nil {
		t.Fatalf("audit: %v", err)
	}
	initN := 0
	for _, e := range events {
		if e.Kind == registry.AuditInit {
			initN++
		}
	}
	if initN != 1 {
		t.Fatalf("expected 1 init audit on real open, got %d (%+v)", initN, events)
	}

	// Second session: reuse, no error.
	sinkB := &fanoutSink{}
	selB, err := d.ForwardSAP(ctx, "real-b", sinkB, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("second select: %v", err)
	}
	var stateB map[string]any
	if err := json.Unmarshal(selB, &stateB); err != nil {
		t.Fatalf("unmarshal B: %v", err)
	}
	if stateB["opened"] != true {
		t.Fatalf("expected opened=true on reselect, got %+v", stateB)
	}
	events, _ = d.Reg.ListAuditEvents(proj.ID)
	initN = 0
	for _, e := range events {
		if e.Kind == registry.AuditInit {
			initN++
		}
	}
	if initN != 1 {
		t.Fatalf("expected still 1 init after second real select, got %d", initN)
	}

	// listTracks should succeed (content may be empty on minimal mlt).
	if _, err := d.ForwardSAP(ctx, "real-b", sinkB, "edit.listTracks", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("listTracks after real open: %v", err)
	}
}

func TestProjectOpen_BootReconcileMarksKilledChildCrashed(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "reconcile-proj"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	sink := &fanoutSink{}
	if _, err := d.ForwardSAP(ctx, "s1", sink, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID})); err != nil {
		t.Fatalf("select: %v", err)
	}
	instances, err := d.Reg.ListProcessInstancesByProject(proj.ID)
	if err != nil || len(instances) == 0 {
		t.Fatalf("expected instance, got %+v err=%v", instances, err)
	}
	pi := instances[0]
	proc, err := os.FindProcess(pi.PID)
	if err != nil {
		t.Fatalf("find process: %v", err)
	}
	if err := proc.Kill(); err != nil {
		t.Fatalf("kill child: %v", err)
	}
	// Wait until OS reaps; reconcile checks PID liveness.
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if err := proc.Signal(os.Signal(nil)); err != nil {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	// Give kernel a moment; on Linux FindProcess always succeeds so just sleep.
	time.Sleep(200 * time.Millisecond)

	outcomes, err := d.Reconcile(ctx)
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	_ = outcomes
	updated, err := d.Reg.GetProcessInstance(pi.ID)
	if err != nil {
		t.Fatalf("get instance: %v", err)
	}
	if updated.Status != registry.StatusCrashed {
		t.Fatalf("expected crashed after kill+reconcile, got %s", updated.Status)
	}
}
