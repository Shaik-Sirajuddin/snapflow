package mcpadapter_test

// TestMCPAdapter_PhaseC_DifferentProjectsIsolation is the real, MCP-level
// proof requested by 11-e2e-scenario-tests.md's Phase C: two independent
// MCP/SSE client sessions, project.select-ed into TWO different real
// projects (two real, independently launched sap-rust processes), proving
// isolation actually holds under real concurrent load rather than just by
// architectural argument:
//
//  1. Agent 1 (bound to project A) cannot import a real media file located
//     inside project B's root, and referencing a real clipId that only
//     exists in project B's process also fails cleanly -- not a crash or a
//     hang.
//  2. Agent 1 never receives a notification meant for project B, even
//     though Agent 2 actively mutates project B during the wait window.
//  3. Both agents run real, concurrent file.export jobs (real melt
//     subprocesses) on their own distinct real projects and both produce
//     correct, distinct output files, verified with real ffprobe.

import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"

	"github.com/mark3labs/mcp-go/server"
)

func TestMCPAdapter_PhaseC_DifferentProjectsIsolation(t *testing.T) {
	binPath := realSapRustBinary(t)
	requireFFmpegTools(t)

	workdir := t.TempDir()
	sourceA := generateTestSource(t, t.TempDir(), 2) // 2s -> 60 frames
	sourceB := generateTestSource(t, t.TempDir(), 3) // 3s -> 90 frames, deliberately different

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

	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()

	// Two real projects, each launched as its own real, independent
	// sap-rust process (separate PID, separate socket, separate in-memory
	// MltBackend state).
	projA, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "phase-c-project-a"})
	if err != nil {
		t.Fatalf("create project A: %v", err)
	}
	projB, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "phase-c-project-b"})
	if err != nil {
		t.Fatalf("create project B: %v", err)
	}
	piA, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: projA.ID})
	if err != nil {
		t.Fatalf("launch real sap-rust for project A: %v", err)
	}
	piB, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: projB.ID})
	if err != nil {
		t.Fatalf("launch real sap-rust for project B: %v", err)
	}
	if piA.PID == piB.PID || piA.SocketPath == piB.SocketPath {
		t.Fatalf("expected two distinct real processes, got instance A=%+v instance B=%+v", piA, piB)
	}
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), piA.ID) })
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), piB.ID) })

	mcpServer := mcpadapter.New(d)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	agent1 := newMCPAgent(t, ctx, testServer.URL+"/sse") // bound to project A
	agent2 := newMCPAgent(t, ctx, testServer.URL+"/sse") // bound to project B
	// Registered after testServer's defer above, so LIFO closes these
	// (dropping their SSE connections) before testServer.Close() runs --
	// see mcpAgent.Close's doc comment for why the ordering matters.
	defer agent1.Close()
	defer agent2.Close()

	if got := agent1.sapCall("project.select", map[string]any{"projectId": projA.ID}); got["projectId"] != projA.ID {
		t.Fatalf("agent1 project.select: expected projectId %s, got %+v", projA.ID, got)
	}
	if got := agent2.sapCall("project.select", map[string]any{"projectId": projB.ID}); got["projectId"] != projB.ID {
		t.Fatalf("agent2 project.select: expected projectId %s, got %+v", projB.ID, got)
	}

	// Put a real readable file inside project B's root. Agent 1 must reject
	// this exact path because its bound project root is project A, proving
	// file.import's sandbox is per project rather than daemon-global.
	projectBAsset := filepath.Join(projB.RootDir, "assets", "project-b-source.mp4")
	if err := os.MkdirAll(filepath.Dir(projectBAsset), 0o755); err != nil {
		t.Fatalf("create project B asset directory: %v", err)
	}
	media, err := os.ReadFile(sourceB)
	if err != nil {
		t.Fatalf("read generated project B source: %v", err)
	}
	if err := os.WriteFile(projectBAsset, media, 0o644); err != nil {
		t.Fatalf("copy source into project B root: %v", err)
	}

	// Give each project one real track + real clip to work with.
	agent1.sapCall("edit.addTrack", map[string]any{"kind": "video"})
	entryA := agent1.sapCall("playlist.append", map[string]any{"source": map[string]any{"path": sourceA}})
	clipA := agent1.sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": entryA["index"]},
	})
	clipAID, _ := clipA["clipId"].(string)
	if clipAID == "" {
		t.Fatalf("expected a real clipId from project A's edit.appendClip, got %+v", clipA)
	}

	agent2.sapCall("edit.addTrack", map[string]any{"kind": "video"})
	entryB := agent2.sapCall("playlist.append", map[string]any{"source": map[string]any{"path": sourceB}})
	// sap-rust's clip ids are per-process sequential ("clip-1", "clip-2",
	// ...) -- since project A and project B are two totally separate
	// processes, their local counters start from the same base and would
	// coincidentally produce the *same* clipId string for each project's
	// first clip. To make the cross-project reference below unambiguous
	// (a real foreign id, not one that happens to also be valid in project
	// A for an unrelated reason), project B appends a second clip and uses
	// *that* one's id -- project A only ever creates one clip, so this id
	// cannot coincidentally exist there too.
	_ = agent2.sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": entryB["index"]},
	})
	clipB := agent2.sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": entryB["index"]},
	})
	clipBID, _ := clipB["clipId"].(string)
	if clipBID == "" {
		t.Fatalf("expected a real clipId from project B's edit.appendClip, got %+v", clipB)
	}
	if clipBID == clipAID {
		t.Fatalf("test setup bug: clipBID (%s) collided with clipAID (%s); cross-project test below would be meaningless", clipBID, clipAID)
	}

	// --- Test 1: cross-project file and resource references fail cleanly
	// (doc 11 Phase C row 1-2 / task's structural-proof requirement) ---
	importErr := agent1.sapCallExpectError("file.import", map[string]any{"path": projectBAsset})
	if !strings.Contains(importErr, "outside project root") {
		t.Fatalf("expected foreign project file.import to explain the sandbox rejection, got: %s", importErr)
	}
	t.Logf("Phase C test 1a OK: agent1 (project A) importing project B's file %s failed cleanly: %s", projectBAsset, importErr)

	// Agent 1 (bound to project A's process) references project B's real
	// clipId. Project A's process has never heard of that id -- it should
	// come back as a clean SAP-level NotFound-style error, not a hang/crash.
	errText := agent1.sapCallExpectError("filter.add", map[string]any{
		"clipId":     clipBID,
		"mltService": "brightness",
		"properties": map[string]any{},
	})
	t.Logf("Phase C test 1 OK: agent1 (project A) referencing project B's clipId %s failed cleanly: %s", clipBID, errText)

	// Sanity check the reverse direction too: project A's own clipId does
	// work against project A's own process, so test 1 above is really about
	// cross-project isolation, not a generally-broken filter.add call.
	okResult := agent1.sapCall("filter.add", map[string]any{
		"clipId":     clipAID,
		"mltService": "brightness",
		"properties": map[string]any{},
	})
	if okResult["mltService"] != "brightness" {
		t.Fatalf("expected filter.add to succeed against project A's own clipId, got %+v", okResult)
	}
	if _, found := agent1.waitForSAPNotification("filter.changed", 5*time.Second); !found {
		t.Fatal("agent1 should receive its own filter.changed notification before isolation baseline")
	}

	// --- Test 2: notification isolation (doc 11 Phase C row 3) ---
	baseline := agent1.notificationCount()
	// Agent 2 mutates project B -- agent1 (project A) must receive nothing
	// about it.
	agent2.sapCall("edit.addTrack", map[string]any{"kind": "audio"})
	// Also give agent1 something real to *not* confuse with cross-project
	// leakage: wait the same window a real notification would arrive in
	// (Phase B's fan-out test above already proved same-project delivery is
	// fast), then confirm agent1's notification count didn't move at all.
	time.Sleep(2 * time.Second)
	if got := agent1.notificationCount(); got != baseline {
		leaked := agent1.notificationsSince(baseline)
		t.Fatalf("agent1 (project A) should receive zero notifications from agent2's project B mutation, got %d new: %+v", got-baseline, leaked)
	}
	t.Logf("Phase C test 2 OK: agent1 received zero notifications for agent2's project B mutation over a %s window", 2*time.Second)

	// --- Test 3: concurrent file.export on distinct real projects (doc 11
	// Phase C row 4) ---
	exportA := filepath.Join(workdir, "export-a.mp4")
	exportB := filepath.Join(workdir, "export-b.mp4")

	var wg sync.WaitGroup
	var jobIDA, jobIDB string
	wg.Add(2)
	go func() {
		defer wg.Done()
		res := agent1.sapCall("file.export", map[string]any{"outputPath": exportA, "codec": "libx264", "container": "mp4"})
		jobIDA, _ = res["jobId"].(string)
	}()
	go func() {
		defer wg.Done()
		res := agent2.sapCall("file.export", map[string]any{"outputPath": exportB, "codec": "libx264", "container": "mp4"})
		jobIDB, _ = res["jobId"].(string)
	}()
	wg.Wait()
	if jobIDA == "" || jobIDB == "" {
		t.Fatalf("expected real jobIds from both concurrent exports, got A=%q B=%q", jobIDA, jobIDB)
	}
	if jobIDA == jobIDB {
		t.Fatalf("expected distinct jobIds for two independent projects' exports, got the same id %q for both", jobIDA)
	}

	waitForJobDone := func(agent *mcpAgent, jobID string) map[string]any {
		t.Helper()
		deadline := time.Now().Add(45 * time.Second)
		var last map[string]any
		for time.Now().Before(deadline) {
			last = agent.sapCall("jobs.get", map[string]any{"jobId": jobID})
			if s, _ := last["status"].(string); s != "running" {
				return last
			}
			time.Sleep(300 * time.Millisecond)
		}
		return last
	}

	var jobA, jobB map[string]any
	var jwg sync.WaitGroup
	jwg.Add(2)
	go func() { defer jwg.Done(); jobA = waitForJobDone(agent1, jobIDA) }()
	go func() { defer jwg.Done(); jobB = waitForJobDone(agent2, jobIDB) }()
	jwg.Wait()

	if s, _ := jobA["status"].(string); s != "done" {
		t.Fatalf("expected project A's concurrent export to reach status=done, last: %+v", jobA)
	}
	if s, _ := jobB["status"].(string); s != "done" {
		t.Fatalf("expected project B's concurrent export to reach status=done, last: %+v", jobB)
	}

	if _, err := os.Stat(exportA); err != nil {
		t.Fatalf("project A's exported file should exist at %s: %v", exportA, err)
	}
	if _, err := os.Stat(exportB); err != nil {
		t.Fatalf("project B's exported file should exist at %s: %v", exportB, err)
	}

	hasVideoA, hasAudioA, durationA := ffprobeStreamsAndDuration(t, exportA)
	hasVideoB, hasAudioB, durationB := ffprobeStreamsAndDuration(t, exportB)
	if !hasVideoA || !hasAudioA {
		t.Fatalf("project A export should have real h264 video + audio streams, got video=%v audio=%v", hasVideoA, hasAudioA)
	}
	if !hasVideoB || !hasAudioB {
		t.Fatalf("project B export should have real h264 video + audio streams, got video=%v audio=%v", hasVideoB, hasAudioB)
	}
	// Project A's source was 2s, project B's was 3s -- distinct, correct
	// durations prove these are two real, independent renders, not one
	// project's output copied/aliased for both.
	if diff := durationA - 2.0; diff > 0.5 || diff < -0.5 {
		t.Fatalf("project A export duration %.3fs should be close to its real 2s source, got a mismatch suggesting cross-project contamination", durationA)
	}
	// Project B's timeline has two 3s clips appended back to back (the
	// extra one exists purely to give it a clipId that can't coincide with
	// project A's, per the comment above) -- so its real expected duration
	// is ~6s, not 3s.
	if diff := durationB - 6.0; diff > 0.5 || diff < -0.5 {
		t.Fatalf("project B export duration %.3fs should be close to its real ~6s (two 3s clips) timeline, got a mismatch suggesting cross-project contamination", durationB)
	}
	t.Logf("Phase C test 3 OK: two real concurrent melt exports completed independently -- A: %s (%.3fs) B: %s (%.3fs)", exportA, durationA, exportB, durationB)
}
