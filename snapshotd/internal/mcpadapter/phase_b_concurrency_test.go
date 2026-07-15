package mcpadapter_test

// TestMCPAdapter_PhaseB_SameProjectConcurrency is the real, MCP-level proof
// requested by 11-e2e-scenario-tests.md's Phase B: two independent MCP/SSE
// client sessions, both project.select-ed into the SAME running real
// sap-rust process (FfiBackend, driving the real Qt/C++ Shotcut engine --
// not a mock), exercising:
//
//  1. Notification fan-out: a mutation Agent 1 requests is delivered to
//     Agent 2's live SSE stream as a real "sap.notification" even though
//     Agent 2 never asked for it, through the full MCP round trip -- not
//     just proven at the sap-rust socket level like the earlier
//     *_realsaprust_test.go files.
//  2. The shared, session-independent undo_depth counter: Agent 1's
//     project.undo() (a real Qt QUndoStack operation on the video-track add
//     immediately preceding it) is observed by Agent 2 via
//     project.getState() even though Agent 2 didn't call undo -- proving
//     project state (not just the connection) is shared. Deliberately
//     exercised on a track add rather than the filter work in row 3 below:
//     FfiBackend's filter.add/filter.setProperty push no QUndoCommand of
//     their own (see sap_ffi.cpp), so undoing/redoing here first keeps this
//     proof from colliding with (and collaterally discarding) the filter
//     state that row 3 depends on.
//  3. Last-write-wins on a shared mutable resource: Agent 2 sets one
//     brightness filter property, Agent 1 immediately replaces it, and the
//     project's serialized MLT XML must contain Agent 1's later value.
//  5. Project-scoped (not session-scoped) job visibility: a file.export job
//     started by Agent 1 appears in Agent 2's jobs.list result, then remains
//     queryable through jobs.get until it completes.

import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"

	"github.com/mark3labs/mcp-go/server"
)

func TestMCPAdapter_PhaseB_SameProjectConcurrency(t *testing.T) {
	binPath := realSapRustBinary(t)
	requireFFmpegTools(t)

	workdir := t.TempDir()
	source := generateTestSource(t, workdir, 2) // 2s @ 30fps -> 60 frames

	cfg := config.Config{
		HomeDir:         t.TempDir(),
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          filepath.Join(t.TempDir(), "run"),
		SnapshotBinPath: binPath,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := daemon.New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	// One real project, launched once -- one real sap-rust process that
	// both agents will bind to.
	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "phase-b-same-project"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch real sap-rust: %v", err)
	}
	if pi.Status != "ready" {
		t.Fatalf("expected ready status, got %s", pi.Status)
	}
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), pi.ID) })

	mcpServer := mcpadapter.New(d)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	// Two separate MCP/SSE client connections -- two separate MCP sessions
	// -- both bound to the SAME projectId, per doc 05's multi-client model.
	agent1 := newMCPAgent(t, ctx, testServer.URL+"/sse")
	agent2 := newMCPAgent(t, ctx, testServer.URL+"/sse")
	// Registered after testServer's defer above, so LIFO closes these
	// (dropping their SSE connections) before testServer.Close() runs --
	// see mcpAgent.Close's doc comment for why the ordering matters.
	defer agent1.Close()
	defer agent2.Close()

	sel1 := agent1.sapCall("project.select", map[string]any{"projectId": proj.ID})
	if sel1["projectId"] != proj.ID {
		t.Fatalf("agent1 project.select: expected projectId %s, got %+v", proj.ID, sel1)
	}
	sel2 := agent2.sapCall("project.select", map[string]any{"projectId": proj.ID})
	if sel2["projectId"] != proj.ID {
		t.Fatalf("agent2 project.select: expected projectId %s, got %+v", proj.ID, sel2)
	}

	// --- Step 1: notification fan-out (doc 11 Phase B row 1) ---
	// Agent 1 mutates; Agent 2 (idle, subscribed) must see it without
	// having requested anything.
	agent1.sapCall("edit.addTrack", map[string]any{"kind": "audio"})
	fields, found := agent2.waitForSAPNotification("edit.changed", 5*time.Second)
	if !found {
		t.Fatalf("agent2 should have received a fanned-out edit.changed sap.notification for agent1's edit.addTrack")
	}
	notifParams, _ := fields["params"].(map[string]any)
	if reason, _ := notifParams["reason"].(string); reason != "addTrack" {
		t.Fatalf("expected the fanned-out edit.changed notification's reason to be addTrack, got %+v", fields)
	}
	t.Logf("Phase B step 1 OK: agent2 received fan-out notification for agent1's mutation it never requested: %+v", fields)

	// Add the video track that clip work below will land on. This (like
	// the audio addTrack above) is a real Timeline::AddTrackCommand on
	// mw->undoStack() -- it is the operation step 4's undo/redo proof
	// below exercises. Note: unlike edit.addTrack/edit.appendClip,
	// FfiBackend's filter.add/filter.setProperty (used further down for
	// the last-write-wins race) push no QUndoCommand of their own -- see
	// sap_ffi.cpp's sap_filter_add/sap_filter_set_property doc comments.
	// Doing the undo/redo proof here, on the track add, rather than after
	// the filter race, means it neither depends on nor disturbs filter
	// state that isn't on the undo stack in the first place.
	agent1.sapCall("edit.addTrack", map[string]any{"kind": "video"})

	// --- Step 2: shared undo_depth counter (doc 11 Phase B row 4) ---
	before := agent1.sapCall("project.getState", map[string]any{})
	beforeUndo, _ := before["undoDepth"].(float64)
	if beforeUndo == 0 {
		t.Fatalf("expected a nonzero undoDepth after the mutations above, got %+v", before)
	}
	agent1.sapCall("project.undo", map[string]any{})
	// Agent 2 never called undo -- it must still observe the shared
	// counter's new value via its own project.getState call.
	after := agent2.sapCall("project.getState", map[string]any{})
	afterUndo, _ := after["undoDepth"].(float64)
	afterRedo, _ := after["redoDepth"].(float64)
	if afterUndo != beforeUndo-1 {
		t.Fatalf("expected agent2 to observe the shared undoDepth decremented by agent1's undo (%v -> %v), got %v", beforeUndo, beforeUndo-1, afterUndo)
	}
	if afterRedo < 1 {
		t.Fatalf("expected agent2 to observe redoDepth incremented by agent1's undo, got %v", afterRedo)
	}
	t.Logf("Phase B step 2 OK: agent2 observed shared undoDepth %v->%v via project.getState after agent1's project.undo (agent2 never called undo itself)", beforeUndo, afterUndo)
	// Unlike the removed MltBackend's fake depth counter, FfiBackend's
	// project.undo is real (Qt's QUndoStack) -- it genuinely reverted
	// agent1's video-track add. project.redo restores it so the clip
	// work below (which appends to trackIndex 0, the just-restored video
	// track) sees the same timeline shape either way.
	agent1.sapCall("project.redo", map[string]any{})

	// Set up a real clip both agents can race on: one appended real
	// source clip on the video track (2s @ 30fps = 60 frames, so inFrame
	// values below 60 are all valid per edit.trimClipIn's own
	// validation).
	appended := agent1.sapCall("playlist.append", map[string]any{"source": map[string]any{"path": source}})
	clip := agent1.sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": appended["index"]},
	})
	clipID, _ := clip["clipId"].(string)
	if clipID == "" {
		t.Fatalf("expected a real clipId from edit.appendClip, got %+v", clip)
	}
	filter := agent1.sapCall("filter.add", map[string]any{
		"clipId":     clipID,
		"mltService": "brightness",
		"properties": map[string]any{},
	})
	filterIndex, _ := filter["filterIndex"].(float64)

	// --- Step 3: last-write-wins race on the same clip (doc 11 Phase B
	// rows 2-3). Agent 2 writes first (A=0.25), Agent 1 immediately writes
	// B=0.75 to the same filter property, with no undo/redo in between
	// (filter.setProperty isn't itself undoable in the current FfiBackend
	// -- see the comment above step 2). Exporting below serializes the
	// real MLT project state, which is the read-back proving B won.
	const valueA = 0.25
	const valueB = 0.75
	agent2.sapCall("filter.setProperty", map[string]any{
		"clipId": clipID, "filterIndex": filterIndex, "property": "level", "value": valueA,
	})
	agent1.sapCall("filter.setProperty", map[string]any{
		"clipId": clipID, "filterIndex": filterIndex, "property": "level", "value": valueB,
	})

	// --- Step 5: project-scoped (not session-scoped) job visibility (doc
	// 11 Phase B row 5) ---
	exportRes := agent1.sapCall("file.export", map[string]any{
		"outputPath": "phase-b-export.mp4",
		"codec":      "libx264",
		"container":  "mp4",
	})
	jobID, _ := exportRes["jobId"].(string)
	if jobID == "" {
		t.Fatalf("expected a real jobId from agent1's file.export, got %+v", exportRes)
	}
	// Agent 2 -- a different session -- must see this job in its project-
	// scoped queue before it polls the individual id, proving it did not
	// merely receive a job id out of band.
	jobs := agent2.sapCallList("jobs.list", map[string]any{})
	foundJob := false
	for _, listed := range jobs {
		if listedID, _ := listed["jobId"].(string); listedID == jobID {
			foundJob = true
			break
		}
	}
	if !foundJob {
		t.Fatalf("agent2 jobs.list should include agent1's project job %s, got %+v", jobID, jobs)
	}

	// Agent 2 then polls the same shared job to completion.
	status := "running"
	var lastJob map[string]any
	deadline := time.Now().Add(45 * time.Second)
	for time.Now().Before(deadline) {
		lastJob = agent2.sapCall("jobs.get", map[string]any{"jobId": jobID})
		status, _ = lastJob["status"].(string)
		if status != "running" {
			break
		}
		time.Sleep(300 * time.Millisecond)
	}
	if status != "done" {
		t.Fatalf("expected agent2's jobs.get polling on agent1's job to reach status=done, last: %+v", lastJob)
	}
	// file.export writes to an explicit scratch outputPath, not
	// <projectRoot>/project.mlt -- project.save is the real, explicit
	// primitive (Controller::saveXML()) that persists the live session
	// there, matching real Shotcut's "must save to persist" semantics.
	agent1.sapCall("project.save", map[string]any{})
	projectXML, err := os.ReadFile(filepath.Join(proj.RootDir, "project.mlt"))
	if err != nil {
		t.Fatalf("read exported project XML: %v", err)
	}
	if !strings.Contains(string(projectXML), `<property name="level">0.75</property>`) ||
		strings.Contains(string(projectXML), `<property name="level">0.25</property>`) {
		t.Fatalf("last-write-wins should serialize only agent1's later brightness value, XML=%s", projectXML)
	}
	t.Logf("Phase B step 3 OK: last-write-wins confirmed, filter level=%.2f (agent1's write, not agent2's %.2f)", valueB, valueA)
	t.Logf("Phase B step 5 OK: agent2 discovered agent1's export job (%s) via jobs.list and observed it reach status=done via jobs.get on a different session", jobID)
}
