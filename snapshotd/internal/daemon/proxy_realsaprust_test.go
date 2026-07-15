package daemon

import (
	"context"
	"encoding/json"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"snapshotd/internal/config"
)

// realSapRustBinary locates the real, production child binary: the Qt/
// `real_ffi`-linked `shotcut` binary (shotcut/CMakeLists.txt's
// corrosion_import_crate(... FEATURES real_ffi), sap-rust/README.md's
// "Real FFI" section), under shotcut/build*/src/shotcut relative to this
// repo's root. This is deliberately NOT the standalone
// sap-rust/target/{debug,release}/sap-rust binary -- since the MltBackend
// removal, that binary only ever runs MockBackend (no real ffprobe/melt),
// which cannot back this file's file.export/file.probe-touching
// assertions. If no such build exists yet, the test is skipped, not
// failed: this package's `go test ./...` must not require a full Qt build
// to exist. In this checkout it does exist, so these tests actually prove
// the daemon -> procmgr -> real headless Shotcut/FfiBackend chain end to
// end.
func realSapRustBinary(t *testing.T) string {
	t.Helper()
	repoRoot := filepath.Join("..", "..", "..")
	matches, err := filepath.Glob(filepath.Join(repoRoot, "shotcut", "build*", "src", "shotcut"))
	if err == nil {
		for _, candidate := range matches {
			if info, statErr := os.Stat(candidate); statErr == nil && !info.IsDir() {
				abs, absErr := filepath.Abs(candidate)
				if absErr != nil {
					t.Fatalf("abs path: %v", absErr)
				}
				return abs
			}
		}
	}
	t.Skip("real Qt/real_ffi shotcut binary not found under shotcut/build*/src/shotcut; run `cmake -S shotcut -B shotcut/build-real-ffi -G Ninja && ninja -C shotcut/build-real-ffi` first to run this integration test")
	return ""
}

type fanoutSink struct {
	mu     sync.Mutex
	events []string
}

func (s *fanoutSink) Notify(method string, params json.RawMessage) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.events = append(s.events, method)
}

func (s *fanoutSink) count() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.events)
}

// TestForwardSAP_RealSapRust_EndToEnd launches the real sap-rust binary via
// the daemon's process manager, then drives the generic SAP proxy exactly
// as internal/sdp and internal/mcpadapter do: project.select to bind a
// session, then opaque edit.* calls forwarded verbatim, asserting the
// results reflect real (mutated) MockBackend state -- and that a second
// session bound to the same project receives the fanned-out edit.changed
// notification for a mutation it did not itself make.
func TestForwardSAP_RealSapRust_EndToEnd(t *testing.T) {
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
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "proxy-e2e"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}

	// Default (headless omitted) must launch with SNAPSHOT_HEADLESS=1.
	pi, err := d.Launch(ctx, LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch real sap-rust: %v", err)
	}
	if pi.Status != "ready" {
		t.Fatalf("expected ready status from real sap-rust, got %s", pi.Status)
	}
	if pi.Token == "" {
		t.Fatalf("expected a persisted per-launch token")
	}

	sinkA := &fanoutSink{}
	sinkB := &fanoutSink{}

	// session-a: project.select, then mutate.
	selectRaw, err := d.ForwardSAP(ctx, "session-a", sinkA, "project.select", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("project.select (session-a): %v", err)
	}
	var state map[string]any
	if err := json.Unmarshal(selectRaw, &state); err != nil {
		t.Fatalf("unmarshal project.select result: %v", err)
	}
	if state["projectId"] != proj.ID {
		t.Fatalf("expected real ProjectState.projectId == %s, got %+v", proj.ID, state)
	}

	// session-b: also bind to the same project (must share the pooled
	// connection, not open a second one).
	if _, err := d.ForwardSAP(ctx, "session-b", sinkB, "project.select", mustJSON(t, map[string]any{"projectId": proj.ID})); err != nil {
		t.Fatalf("project.select (session-b): %v", err)
	}

	// Opaque forwarded mutation from session-a: edit.addTrack. This package
	// has zero compiled-in knowledge of "kind"/"trackIndex" -- it is just
	// forwarding whatever params/method the caller supplied.
	addRaw, err := d.ForwardSAP(ctx, "session-a", sinkA, "edit.addTrack", mustJSON(t, map[string]any{"kind": "video"}))
	if err != nil {
		t.Fatalf("edit.addTrack: %v", err)
	}
	var track map[string]any
	if err := json.Unmarshal(addRaw, &track); err != nil {
		t.Fatalf("unmarshal track: %v", err)
	}
	if track["kind"] != "video" {
		t.Fatalf("expected real track result with kind=video, got %+v", track)
	}

	// Read back via session-b: proves the mutation landed in the one real
	// shared backend state, not some per-session mock.
	listRaw, err := d.ForwardSAP(ctx, "session-b", sinkB, "edit.listTracks", nil)
	if err != nil {
		t.Fatalf("edit.listTracks: %v", err)
	}
	var tracks []map[string]any
	if err := json.Unmarshal(listRaw, &tracks); err != nil {
		t.Fatalf("unmarshal tracks: %v", err)
	}
	if len(tracks) != 1 || tracks[0]["kind"] != "video" {
		t.Fatalf("expected session-b to see session-a's real mutation, got %+v", tracks)
	}

	// Fan-out: both sessions bound to the project should observe the
	// edit.changed notification sap-rust broadcasts on a successful mutation
	// (per sap-rust/src/server.rs's build_op), even session-b which didn't
	// make the call.
	deadline := time.Now().Add(3 * time.Second)
	for (sinkA.count() == 0 || sinkB.count() == 0) && time.Now().Before(deadline) {
		time.Sleep(20 * time.Millisecond)
	}
	if sinkA.count() == 0 {
		t.Fatalf("expected session-a to observe a fanned-out notification too (broadcast is not requester-exclusive)")
	}
	if sinkB.count() == 0 {
		t.Fatalf("expected session-b to observe the fanned-out edit.changed notification from session-a's mutation")
	}

	// Calling before project.select fails cleanly.
	if _, err := d.ForwardSAP(ctx, "session-fresh", &fanoutSink{}, "edit.listTracks", nil); err == nil {
		t.Fatalf("expected error calling a project-scoped method before project.select")
	}

	// Clean up.
	d.UnbindSession("session-a")
	d.UnbindSession("session-b")
	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}
}

func mustJSON(t *testing.T, v any) json.RawMessage {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return b
}

// shortRunDir returns a fresh temp directory for procmgr's control sockets,
// deliberately NOT nested under t.TempDir() (which embeds t.Name()): a long
// test function name joined with ".../run/<16-hex>.sock" can blow past the
// ~104-byte AF_UNIX sun_path limit, failing Launch before sap-rust is even
// spawned. RunDir is the only config path that goes into a socket address;
// HomeDir/ProjectsRoot have no such limit and can keep using t.TempDir().
func shortRunDir(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "sapd-run-")
	if err != nil {
		t.Fatalf("mkdir short run dir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	return dir
}

// TestForwardSAP_RealSapRust_ProjectSwitchGuard proves the harness guard
// end-to-end through the real daemon.ForwardSAP entry point (the same one
// internal/sdp and internal/mcpadapter use): a session already bound to one
// project is rejected when it tries to project.select a different one
// without an intervening project.exit, and project.exit -- handled locally
// by ForwardSAP, not forwarded to the shared sap-rust connection, see its
// doc comment -- correctly clears the binding so a later select to a
// different project succeeds.
func TestForwardSAP_RealSapRust_ProjectSwitchGuard(t *testing.T) {
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
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	projA, err := d.CreateProject(ctx, CreateProjectParams{Name: "switch-guard-a"})
	if err != nil {
		t.Fatalf("create project a: %v", err)
	}
	projB, err := d.CreateProject(ctx, CreateProjectParams{Name: "switch-guard-b"})
	if err != nil {
		t.Fatalf("create project b: %v", err)
	}

	piA, err := d.Launch(ctx, LaunchParams{ProjectID: projA.ID})
	if err != nil {
		t.Fatalf("launch project a: %v", err)
	}
	piB, err := d.Launch(ctx, LaunchParams{ProjectID: projB.ID})
	if err != nil {
		t.Fatalf("launch project b: %v", err)
	}

	sink := &fanoutSink{}

	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "project.select", mustJSON(t, map[string]any{"projectId": projA.ID})); err != nil {
		t.Fatalf("project.select projA: %v", err)
	}

	// Reselecting the SAME project must stay a no-op success.
	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "project.select", mustJSON(t, map[string]any{"projectId": projA.ID})); err != nil {
		t.Fatalf("reselecting projA should succeed: %v", err)
	}

	// Switching to project B without exiting project A must be rejected.
	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "project.select", mustJSON(t, map[string]any{"projectId": projB.ID})); err == nil {
		t.Fatalf("expected project.select to projB to be rejected while still bound to projA")
	}

	// Still usable against project A -- the rejected attempt must not have
	// disturbed the existing binding.
	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "edit.listTracks", nil); err != nil {
		t.Fatalf("session should still be bound to projA after the rejected switch: %v", err)
	}

	// project.exit, then select project B, must succeed.
	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "project.exit", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("project.exit: %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "project.select", mustJSON(t, map[string]any{"projectId": projB.ID})); err != nil {
		t.Fatalf("project.select projB after exit should succeed: %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "session-switch", sink, "edit.listTracks", nil); err != nil {
		t.Fatalf("session should now be bound to projB: %v", err)
	}

	d.UnbindSession("session-switch")
	if err := d.CloseInstance(ctx, piA.ID); err != nil {
		t.Fatalf("close instance a: %v", err)
	}
	if err := d.CloseInstance(ctx, piB.ID); err != nil {
		t.Fatalf("close instance b: %v", err)
	}
}

// TestForwardSAP_RealSapRust_PhaseB_SameProjectConcurrentSessions drives two
// independent Go-level sessions bound to the SAME real sap-rust project
// through the exact daemon.ForwardSAP entry point a real MCP client uses,
// proving the same-project multi-agent policies from
// memory/head/gen/rust-fork/11-e2e-scenario-tests.md's Phase B table:
//   - notification fan-out for a mutation the other session did not request
//   - filter.setProperty last-write-wins on a clip shared by both sessions
//   - a single shared linear undo stack, not one per session
//   - jobs.list visibility for a job started by the other session
//
// This drives the two sessions sequentially (not with real goroutine-level
// concurrency) because the policies under test are about shared *state*
// visibility across sessions, not about race-safety of simultaneous calls;
// sap-rust's single dispatch actor (see server.rs) already serializes all
// mutating calls regardless of which session issued them.
func TestForwardSAP_RealSapRust_PhaseB_SameProjectConcurrentSessions(t *testing.T) {
	binPath := realSapRustBinary(t)

	cfg := config.Config{
		HomeDir:         t.TempDir(),
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          shortRunDir(t),
		SnapshotBinPath: binPath,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, CreateProjectParams{Name: "phase-b"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi, err := d.Launch(ctx, LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch: %v", err)
	}

	sinkA := &fanoutSink{}
	sinkB := &fanoutSink{}
	if _, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "project.select", mustJSON(t, map[string]any{"projectId": proj.ID})); err != nil {
		t.Fatalf("project.select (A): %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "phaseb-b", sinkB, "project.select", mustJSON(t, map[string]any{"projectId": proj.ID})); err != nil {
		t.Fatalf("project.select (B): %v", err)
	}

	// -- Fan-out: session A's edits must reach session B, unrequested. --
	beforeB := sinkB.count()
	if _, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "generator.createTitle", mustJSON(t, map[string]any{"text": "phase-b-clip"})); err != nil {
		t.Fatalf("generator.createTitle: %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "edit.addTrack", mustJSON(t, map[string]any{"kind": "video"})); err != nil {
		t.Fatalf("edit.addTrack: %v", err)
	}
	clipRaw, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "edit.appendClip", mustJSON(t, map[string]any{
		"trackIndex": 0,
		"source":     map[string]any{"playlistIndex": 0},
	}))
	if err != nil {
		t.Fatalf("edit.appendClip: %v", err)
	}
	var clip map[string]any
	if err := json.Unmarshal(clipRaw, &clip); err != nil {
		t.Fatalf("unmarshal clip: %v", err)
	}
	clipID, _ := clip["clipId"].(string)
	if clipID == "" {
		t.Fatalf("expected clipId in edit.appendClip result, got %+v", clip)
	}

	deadline := time.Now().Add(3 * time.Second)
	for sinkB.count() == beforeB && time.Now().Before(deadline) {
		time.Sleep(20 * time.Millisecond)
	}
	if sinkB.count() == beforeB {
		t.Fatalf("expected session B to observe session A's edits it never asked for (fan-out is not requester-exclusive)")
	}

	// -- filter.setProperty last-write-wins on the shared clip. --
	if _, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "filter.add", mustJSON(t, map[string]any{
		"clipId": clipID, "mltService": "brightness",
	})); err != nil {
		t.Fatalf("filter.add: %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "filter.setProperty", mustJSON(t, map[string]any{
		"clipId": clipID, "filterIndex": 0, "property": "level", "value": 0.25,
	})); err != nil {
		t.Fatalf("filter.setProperty (A, first): %v", err)
	}
	// Session B writes last: last-write-wins means B's value must stick,
	// regardless of which session reads it back afterward.
	if _, err := d.ForwardSAP(ctx, "phaseb-b", sinkB, "filter.setProperty", mustJSON(t, map[string]any{
		"clipId": clipID, "filterIndex": 0, "property": "level", "value": 0.75,
	})); err != nil {
		t.Fatalf("filter.setProperty (B, last): %v", err)
	}
	listRaw, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "filter.list", mustJSON(t, map[string]any{"clipId": clipID}))
	if err != nil {
		t.Fatalf("filter.list: %v", err)
	}
	var filters []map[string]any
	if err := json.Unmarshal(listRaw, &filters); err != nil {
		t.Fatalf("unmarshal filters: %v", err)
	}
	if len(filters) != 1 {
		t.Fatalf("expected exactly 1 filter, got %+v", filters)
	}
	props, _ := filters[0]["properties"].(map[string]any)
	if level, ok := props["level"].(float64); !ok || level != 0.75 {
		t.Fatalf("expected last write (session B's 0.75) to win, got properties=%+v", props)
	}

	// -- A single shared linear undo stack, not one per session: session A
	// re-selecting reads the project's current undo_depth; session B's
	// project.undo must visibly decrement it for session A too. --
	stateBefore, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "project.select", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("re-select (A) before undo: %v", err)
	}
	var before map[string]any
	if err := json.Unmarshal(stateBefore, &before); err != nil {
		t.Fatalf("unmarshal state before: %v", err)
	}
	depthBefore, _ := before["undoDepth"].(float64)
	if depthBefore == 0 {
		t.Fatalf("expected a nonzero undo depth after several mutations, got %+v", before)
	}

	if _, err := d.ForwardSAP(ctx, "phaseb-b", sinkB, "project.undo", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("project.undo (B): %v", err)
	}

	stateAfter, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "project.select", mustJSON(t, map[string]any{"projectId": proj.ID}))
	if err != nil {
		t.Fatalf("re-select (A) after undo: %v", err)
	}
	var after map[string]any
	if err := json.Unmarshal(stateAfter, &after); err != nil {
		t.Fatalf("unmarshal state after: %v", err)
	}
	depthAfter, _ := after["undoDepth"].(float64)
	if depthAfter != depthBefore-1 {
		t.Fatalf("expected session B's undo to decrement the ONE shared stack session A observes too: before=%v after=%v", depthBefore, depthAfter)
	}

	// -- jobs.list visibility across sessions: a job session A starts must
	// be visible to session B without session B having started anything. --
	exportRaw, err := d.ForwardSAP(ctx, "phaseb-a", sinkA, "file.export", mustJSON(t, map[string]any{
		"outputPath": "phase-b-export",
	}))
	if err != nil {
		t.Fatalf("file.export: %v", err)
	}
	var exportResult map[string]any
	if err := json.Unmarshal(exportRaw, &exportResult); err != nil {
		t.Fatalf("unmarshal export result: %v", err)
	}
	jobID, _ := exportResult["jobId"].(string)
	if jobID == "" {
		t.Fatalf("expected jobId from file.export, got %+v", exportResult)
	}

	jobsRaw, err := d.ForwardSAP(ctx, "phaseb-b", sinkB, "jobs.list", nil)
	if err != nil {
		t.Fatalf("jobs.list (B): %v", err)
	}
	var jobs []map[string]any
	if err := json.Unmarshal(jobsRaw, &jobs); err != nil {
		t.Fatalf("unmarshal jobs: %v", err)
	}
	found := false
	for _, j := range jobs {
		if j["jobId"] == jobID {
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("expected session B's jobs.list to see session A's export job %s, got %+v", jobID, jobs)
	}

	// Stop it from session B, proving job control (not just visibility) is
	// also cross-session, and keeping this test from waiting on a real melt
	// export to finish.
	if _, err := d.ForwardSAP(ctx, "phaseb-b", sinkB, "jobs.stop", mustJSON(t, map[string]any{"jobId": jobID})); err != nil {
		t.Fatalf("jobs.stop (B): %v", err)
	}

	d.UnbindSession("phaseb-a")
	d.UnbindSession("phaseb-b")
	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}
}

// TestForwardSAP_RealSapRust_PhaseC_DifferentProjectsIsolation drives two
// independent sessions bound to two DIFFERENT real sap-rust projects,
// proving the cross-project isolation policies from
// memory/head/gen/rust-fork/11-e2e-scenario-tests.md's Phase C table:
//   - file.import path rejection is a per-bound-project sandbox, not a
//     global filesystem check: a path that is perfectly valid inside the
//     OTHER session's project root is still rejected for this session
//   - a clipId minted by one project is meaningless (rejected) in the other
//   - notification isolation: a session never observes the other project's
//     edit.changed events
//   - simultaneous file.export jobs on both projects complete independently
func TestForwardSAP_RealSapRust_PhaseC_DifferentProjectsIsolation(t *testing.T) {
	binPath := realSapRustBinary(t)

	cfg := config.Config{
		HomeDir:         t.TempDir(),
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          shortRunDir(t),
		SnapshotBinPath: binPath,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	projA, err := d.CreateProject(ctx, CreateProjectParams{Name: "phase-c-a"})
	if err != nil {
		t.Fatalf("create project a: %v", err)
	}
	projB, err := d.CreateProject(ctx, CreateProjectParams{Name: "phase-c-b"})
	if err != nil {
		t.Fatalf("create project b: %v", err)
	}
	piA, err := d.Launch(ctx, LaunchParams{ProjectID: projA.ID})
	if err != nil {
		t.Fatalf("launch a: %v", err)
	}
	piB, err := d.Launch(ctx, LaunchParams{ProjectID: projB.ID})
	if err != nil {
		t.Fatalf("launch b: %v", err)
	}

	sinkA := &fanoutSink{}
	sinkB := &fanoutSink{}
	if _, err := d.ForwardSAP(ctx, "phasec-a", sinkA, "project.select", mustJSON(t, map[string]any{"projectId": projA.ID})); err != nil {
		t.Fatalf("select A: %v", err)
	}
	if _, err := d.ForwardSAP(ctx, "phasec-b", sinkB, "project.select", mustJSON(t, map[string]any{"projectId": projB.ID})); err != nil {
		t.Fatalf("select B: %v", err)
	}

	// -- Give each project one real clip so filter.add / file.export have
	// something to act on. --
	mkClip := func(session string, sink *fanoutSink, label string) string {
		if _, err := d.ForwardSAP(ctx, session, sink, "generator.createTitle", mustJSON(t, map[string]any{"text": label})); err != nil {
			t.Fatalf("generator.createTitle (%s): %v", session, err)
		}
		if _, err := d.ForwardSAP(ctx, session, sink, "edit.addTrack", mustJSON(t, map[string]any{"kind": "video"})); err != nil {
			t.Fatalf("edit.addTrack (%s): %v", session, err)
		}
		raw, err := d.ForwardSAP(ctx, session, sink, "edit.appendClip", mustJSON(t, map[string]any{
			"trackIndex": 0,
			"source":     map[string]any{"playlistIndex": 0},
		}))
		if err != nil {
			t.Fatalf("edit.appendClip (%s): %v", session, err)
		}
		var clip map[string]any
		if err := json.Unmarshal(raw, &clip); err != nil {
			t.Fatalf("unmarshal clip (%s): %v", session, err)
		}
		id, _ := clip["clipId"].(string)
		if id == "" {
			t.Fatalf("expected clipId (%s), got %+v", session, clip)
		}
		return id
	}

	// mkSecondClipSameTrack appends a second, distinct clip onto the SAME
	// track mkClip's single addTrack call created, rather than calling
	// addTrack again -- real Shotcut's MultitrackModel::addVideoTrack()
	// always *prepends* new video tracks at model index 0 (shifting every
	// existing track's index up by one, matching the GUI's "new track
	// appears on top" convention), so a second addTrack call would target
	// a brand-new, still-empty track at trackIndex 0 instead of adding a
	// second clip next to the first -- silently defeating the "clip with
	// no counterpart in the other project" setup below (both tracks'
	// first clip mint the same positional "t0c0" id).
	mkSecondClipSameTrack := func(session string, sink *fanoutSink, label string) string {
		titleRaw, err := d.ForwardSAP(ctx, session, sink, "generator.createTitle", mustJSON(t, map[string]any{"text": label}))
		if err != nil {
			t.Fatalf("generator.createTitle (%s): %v", session, err)
		}
		var title map[string]any
		if err := json.Unmarshal(titleRaw, &title); err != nil {
			t.Fatalf("unmarshal title (%s): %v", session, err)
		}
		playlistIndex, _ := title["index"].(float64)
		raw, err := d.ForwardSAP(ctx, session, sink, "edit.appendClip", mustJSON(t, map[string]any{
			"trackIndex": 0,
			"source":     map[string]any{"playlistIndex": int(playlistIndex)},
		}))
		if err != nil {
			t.Fatalf("edit.appendClip (%s): %v", session, err)
		}
		var clip map[string]any
		if err := json.Unmarshal(raw, &clip); err != nil {
			t.Fatalf("unmarshal clip (%s): %v", session, err)
		}
		id, _ := clip["clipId"].(string)
		if id == "" {
			t.Fatalf("expected clipId (%s), got %+v", session, clip)
		}
		return id
	}

	// -- Notification isolation: session B must not see session A's edits,
	// checked BEFORE session B does anything of its own. --
	beforeB := sinkB.count()
	clipIDA := mkClip("phasec-a", sinkA, "clip-a")
	time.Sleep(150 * time.Millisecond) // let any (incorrect) cross-broadcast land
	if sinkB.count() != beforeB {
		t.Fatalf("expected session B (project B) to observe zero events from project A's edits, got %d new", sinkB.count()-beforeB)
	}

	clipIDB := mkClip("phasec-b", sinkB, "clip-b")
	// FfiBackend's clip_id is "t{trackIndex}c{clipIndex}", a purely
	// positional encoding (not a per-project sequential counter like the
	// removed MltBackend's), so project B's FIRST clip id can coincide
	// textually with project A's first clip id (both are single-track,
	// single-clip projects, so both are "t0c0"). A second clip appended to
	// B's SAME track gives "t0c1", an id with no counterpart at all in A
	// (which only ever has one clip), genuinely exercising the "unknown
	// clipId" rejection below rather than accidentally hitting A's own
	// same-named clip.
	clipIDB2 := mkSecondClipSameTrack("phasec-b", sinkB, "clip-b-2")

	// -- file.import path rejection is per-bound-project, not global: a
	// file that genuinely exists and is readable under project B's root is
	// still rejected for a session bound to project A. --
	foreignFile := filepath.Join(projB.RootDir, "external.bin")
	if err := os.WriteFile(foreignFile, []byte("not a real media file, just needs to exist"), 0o644); err != nil {
		t.Fatalf("write foreign file: %v", err)
	}
	_, err = d.ForwardSAP(ctx, "phasec-a", sinkA, "file.import", mustJSON(t, map[string]any{"path": foreignFile}))
	if err == nil {
		t.Fatalf("expected file.import of a path under project B's root to be rejected while bound to project A")
	}
	if !strings.Contains(err.Error(), "outside project root") {
		t.Fatalf("expected an 'outside project root' rejection, got: %v", err)
	}

	// -- A clipId minted by project B means nothing in project A. --
	if _, err := d.ForwardSAP(ctx, "phasec-a", sinkA, "filter.add", mustJSON(t, map[string]any{
		"clipId": clipIDB2, "mltService": "brightness",
	})); err == nil {
		t.Fatalf("expected project A to reject project B's clipId %s", clipIDB2)
	}
	// Sanity: the SAME call succeeds for project A's own clip.
	if _, err := d.ForwardSAP(ctx, "phasec-a", sinkA, "filter.add", mustJSON(t, map[string]any{
		"clipId": clipIDA, "mltService": "brightness",
	})); err != nil {
		t.Fatalf("expected project A to accept its own clipId %s: %v", clipIDA, err)
	}
	// clipIDB itself is unused as a rejection probe (see the coincidence
	// note above) but keeping the variable/blank-assign documents why on
	// purpose rather than leaving a silent unused clip creation.
	_ = clipIDB

	// -- Simultaneous file.export jobs on both projects complete
	// independently. --
	exportRawA, err := d.ForwardSAP(ctx, "phasec-a", sinkA, "file.export", mustJSON(t, map[string]any{"outputPath": "export-a"}))
	if err != nil {
		t.Fatalf("file.export A: %v", err)
	}
	exportRawB, err := d.ForwardSAP(ctx, "phasec-b", sinkB, "file.export", mustJSON(t, map[string]any{"outputPath": "export-b"}))
	if err != nil {
		t.Fatalf("file.export B: %v", err)
	}
	var jobA, jobB map[string]any
	if err := json.Unmarshal(exportRawA, &jobA); err != nil {
		t.Fatalf("unmarshal job a: %v", err)
	}
	if err := json.Unmarshal(exportRawB, &jobB); err != nil {
		t.Fatalf("unmarshal job b: %v", err)
	}
	jobIDA, _ := jobA["jobId"].(string)
	jobIDB, _ := jobB["jobId"].(string)
	if jobIDA == "" || jobIDB == "" {
		t.Fatalf("expected both exports to return a jobId: A=%+v B=%+v", jobA, jobB)
	}

	waitDone := func(session string, sink *fanoutSink, jobID string) map[string]any {
		t.Helper()
		deadline := time.Now().Add(60 * time.Second)
		for time.Now().Before(deadline) {
			raw, err := d.ForwardSAP(ctx, session, sink, "jobs.get", mustJSON(t, map[string]any{"jobId": jobID}))
			if err != nil {
				t.Fatalf("jobs.get (%s): %v", session, err)
			}
			var status map[string]any
			if err := json.Unmarshal(raw, &status); err != nil {
				t.Fatalf("unmarshal status (%s): %v", session, err)
			}
			if s, _ := status["status"].(string); s != "running" {
				return status
			}
			time.Sleep(250 * time.Millisecond)
		}
		t.Fatalf("export job %s (%s) did not finish within deadline", jobID, session)
		return nil
	}
	statusA := waitDone("phasec-a", sinkA, jobIDA)
	statusB := waitDone("phasec-b", sinkB, jobIDB)
	if statusA["status"] != "done" {
		t.Fatalf("expected project A's export to succeed independently, got %+v", statusA)
	}
	if statusB["status"] != "done" {
		t.Fatalf("expected project B's export to succeed independently, got %+v", statusB)
	}

	d.UnbindSession("phasec-a")
	d.UnbindSession("phasec-b")
	if err := d.CloseInstance(ctx, piA.ID); err != nil {
		t.Fatalf("close instance a: %v", err)
	}
	if err := d.CloseInstance(ctx, piB.ID); err != nil {
		t.Fatalf("close instance b: %v", err)
	}
}
