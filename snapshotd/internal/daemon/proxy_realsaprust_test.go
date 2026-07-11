package daemon

import (
	"context"
	"encoding/json"
	"log/slog"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"snapshotd/internal/config"
)

// realSapRustBinary locates the actual sap-rust binary built by the other
// engineer's crate (sap-rust/target/{release,debug}/sap-rust, relative to
// this repo's root -- three directories up from internal/daemon). If it
// isn't built yet, the test is skipped rather than failed: sap-rust is
// developed independently and this package must not require it to exist to
// pass `go test ./...`. When it IS present (as it is in this checkout),
// this test proves the generic proxy end-to-end against the real (if
// currently Mock-backed) sap-rust server, not a fixture standing in for it.
func realSapRustBinary(t *testing.T) string {
	t.Helper()
	for _, variant := range []string{"release", "debug"} {
		candidate := filepath.Join("..", "..", "..", "sap-rust", "target", variant, "sap-rust")
		if info, err := os.Stat(candidate); err == nil && !info.IsDir() {
			abs, err := filepath.Abs(candidate)
			if err != nil {
				t.Fatalf("abs path: %v", err)
			}
			return abs
		}
	}
	t.Skip("real sap-rust binary not found under sap-rust/target/{release,debug}/sap-rust; build sap-rust first to run this integration test")
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
