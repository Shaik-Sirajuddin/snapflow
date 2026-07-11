package mcpadapter_test

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"
)

var meanVolumePattern = regexp.MustCompile(`mean_volume:\s*(-?[0-9.]+)\s*dB`)

func meanVolumeDB(t *testing.T, path string) float64 {
	t.Helper()
	out, err := exec.Command(
		"ffmpeg", "-i", path, "-map", "0:a:0", "-af", "volumedetect", "-f", "null", "-",
	).CombinedOutput()
	if err != nil {
		t.Fatalf("measure mean audio volume for %s: %v\n%s", path, err, out)
	}
	match := meanVolumePattern.FindStringSubmatch(string(out))
	if len(match) != 2 {
		t.Fatalf("ffmpeg volumedetect did not report mean volume for %s:\n%s", path, out)
	}
	value, err := strconv.ParseFloat(match[1], 64)
	if err != nil {
		t.Fatalf("parse mean volume %q: %v", match[1], err)
	}
	return value
}

// TestMCPAdapter_AudioSetGainWhenEnabled proves the namespace toggle travels
// from snapshotd's config through child-process launch to MCP discovery and
// real SAP dispatch. The final project XML is emitted by MltBackend and is
// therefore evidence that the real volume filter is part of the export graph,
// not merely accepted by an adapter stub.
func TestMCPAdapter_AudioSetGainWhenEnabled(t *testing.T) {
	binPath := realSapRustBinary(t)
	requireFFmpegTools(t)

	workdir := t.TempDir()
	source := generateTestSource(t, workdir, 2)
	cfg := config.Config{
		HomeDir:         t.TempDir(),
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          filepath.Join(t.TempDir(), "run"),
		SnapshotBinPath: binPath,
		AudioEnabled:    true,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := daemon.New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "audio-enabled"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch sap-rust: %v", err)
	}
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), pi.ID) })

	testServer := server.NewTestServer(mcpadapter.New(d))
	defer testServer.Close()
	agent := newMCPAgent(t, ctx, testServer.URL+"/sse")
	defer agent.Close()

	searchReq := mcp.CallToolRequest{}
	searchReq.Params.Name = "sap.search"
	searchReq.Params.Arguments = map[string]any{"query": "audio.setGain"}
	searchRes, err := agent.client.CallTool(ctx, searchReq)
	if err != nil {
		t.Fatalf("search audio.setGain: %v", err)
	}
	if searchRes.IsError {
		t.Fatalf("audio.setGain search should succeed when enabled: %s", toolResultText(searchRes))
	}
	var matches []map[string]any
	if err := json.Unmarshal([]byte(toolResultText(searchRes)), &matches); err != nil {
		t.Fatalf("decode audio search result: %v", err)
	}
	if len(matches) != 1 || matches[0]["method"] != "audio.setGain" {
		t.Fatalf("expected enabled audio.setGain to be discoverable, got %+v", matches)
	}

	agent.sapCall("project.select", map[string]any{"projectId": proj.ID})
	agent.sapCall("edit.addTrack", map[string]any{"kind": "video"})
	entry := agent.sapCall("playlist.append", map[string]any{"source": map[string]any{"path": source}})
	clip := agent.sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": entry["index"]},
	})
	clipID, _ := clip["clipId"].(string)
	if clipID == "" {
		t.Fatalf("edit.appendClip returned no clipId: %+v", clip)
	}

	exportAndWait := func(outputPath string) {
		t.Helper()
		job := agent.sapCall("file.export", map[string]any{
			"outputPath": outputPath,
			"codec":      "libx264",
			"container":  "mp4",
		})
		jobID, _ := job["jobId"].(string)
		if jobID == "" {
			t.Fatalf("file.export returned no jobId: %+v", job)
		}
		deadline := time.Now().Add(40 * time.Second)
		for {
			status := agent.sapCall("jobs.get", map[string]any{"jobId": jobID})
			if status["status"] == "done" {
				return
			}
			if status["status"] != "running" {
				t.Fatalf("audio export failed: %+v", status)
			}
			if time.Now().After(deadline) {
				t.Fatalf("audio export did not finish: %+v", status)
			}
			time.Sleep(250 * time.Millisecond)
		}
	}

	baseline := filepath.Join(workdir, "audio-baseline.mp4")
	exportAndWait(baseline)

	gain := agent.sapCall("audio.setGain", map[string]any{"clipId": clipID, "db": float64(-9)})
	if gain["mltService"] != "volume" {
		t.Fatalf("audio.setGain should add the real volume filter, got %+v", gain)
	}

	output := filepath.Join(workdir, "audio-gain.mp4")
	exportAndWait(output)
	if _, err := os.Stat(output); err != nil {
		t.Fatalf("audio export file is missing: %v", err)
	}
	hasVideo, hasAudio, _ := ffprobeStreamsAndDuration(t, output)
	if !hasVideo || !hasAudio {
		t.Fatalf("audio gain export should retain real video and audio streams, video=%v audio=%v", hasVideo, hasAudio)
	}
	baselineMean := meanVolumeDB(t, baseline)
	outputMean := meanVolumeDB(t, output)
	if delta := outputMean - baselineMean; delta > -7.0 || delta < -11.0 {
		t.Fatalf(
			"audio.setGain(-9 dB) should lower exported audio by about 9 dB, baseline mean=%.1f dB output mean=%.1f dB delta=%.1f dB",
			baselineMean,
			outputMean,
			delta,
		)
	}
	projectXML, err := os.ReadFile(filepath.Join(proj.RootDir, "project.mlt"))
	if err != nil {
		t.Fatalf("read serialized project: %v", err)
	}
	if !strings.Contains(string(projectXML), "<property name=\"mlt_service\">volume</property>") ||
		!strings.Contains(string(projectXML), "<property name=\"level\">-9.0</property>") {
		t.Fatalf("serialized MLT must contain audio.setGain's volume filter, XML=%s", projectXML)
	}
}

// TestMCPAdapter_AudioRemainingHelpersWhenEnabled covers pan/balance/normalize/
// fade/autofade discovery, dispatch, and serialized MLT filter properties.
func TestMCPAdapter_AudioRemainingHelpersWhenEnabled(t *testing.T) {
	binPath := realSapRustBinary(t)
	requireFFmpegTools(t)

	workdir := t.TempDir()
	source := generateTestSource(t, workdir, 2)
	cfg := config.Config{
		HomeDir:         t.TempDir(),
		ProjectsRoot:    filepath.Join(t.TempDir(), "projects"),
		RunDir:          filepath.Join(t.TempDir(), "run"),
		SnapshotBinPath: binPath,
		AudioEnabled:    true,
	}
	cfg.DBPath = filepath.Join(cfg.HomeDir, "registry.db")
	cfg.ControlSocketPath = filepath.Join(cfg.HomeDir, "control.sock")

	d, err := daemon.New(cfg, slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatalf("new daemon: %v", err)
	}
	d.Proc.ConnectTimeout = 10 * time.Second
	t.Cleanup(func() { _ = d.Close() })

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "audio-helpers"})
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	pi, err := d.Launch(ctx, daemon.LaunchParams{ProjectID: proj.ID})
	if err != nil {
		t.Fatalf("launch sap-rust: %v", err)
	}
	t.Cleanup(func() { _ = d.CloseInstance(context.Background(), pi.ID) })

	testServer := server.NewTestServer(mcpadapter.New(d))
	defer testServer.Close()
	agent := newMCPAgent(t, ctx, testServer.URL+"/sse")
	defer agent.Close()

	for _, method := range []string{
		"audio.setPan",
		"audio.setBalance",
		"audio.setNormalize",
		"audio.setFadeInOut",
		"audio.setAutoFade",
	} {
		searchReq := mcp.CallToolRequest{}
		searchReq.Params.Name = "sap.search"
		searchReq.Params.Arguments = map[string]any{"query": method}
		searchRes, err := agent.client.CallTool(ctx, searchReq)
		if err != nil {
			t.Fatalf("search %s: %v", method, err)
		}
		if searchRes.IsError {
			t.Fatalf("%s search should succeed when enabled: %s", method, toolResultText(searchRes))
		}
		var matches []map[string]any
		if err := json.Unmarshal([]byte(toolResultText(searchRes)), &matches); err != nil {
			t.Fatalf("decode %s search result: %v", method, err)
		}
		if len(matches) != 1 || matches[0]["method"] != method {
			t.Fatalf("expected enabled %s to be discoverable, got %+v", method, matches)
		}
	}

	agent.sapCall("project.select", map[string]any{"projectId": proj.ID})
	agent.sapCall("edit.addTrack", map[string]any{"kind": "video"})
	entry := agent.sapCall("playlist.append", map[string]any{"source": map[string]any{"path": source}})
	clip := agent.sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": entry["index"]},
	})
	clipID, _ := clip["clipId"].(string)
	if clipID == "" {
		t.Fatalf("edit.appendClip returned no clipId: %+v", clip)
	}

	// 2s @ 30fps → 60 frames (in=0, out=59 inclusive).
	const clipLen = int64(60)
	const fadeIn = int64(15)
	const fadeOut = int64(12)

	pan := agent.sapCall("audio.setPan", map[string]any{"clipId": clipID, "pan": 0.25})
	if pan["mltService"] != "panner" {
		t.Fatalf("audio.setPan should add panner, got %+v", pan)
	}

	balance := agent.sapCall("audio.setBalance", map[string]any{"clipId": clipID, "balance": 0.75})
	if balance["mltService"] != "panner" {
		t.Fatalf("audio.setBalance should add panner, got %+v", balance)
	}

	norm1 := agent.sapCall("audio.setNormalize", map[string]any{
		"clipId": clipID, "mode": "1pass", "targetLevel": float64(-18),
	})
	if norm1["mltService"] != "dynamic_loudness" {
		t.Fatalf("audio.setNormalize 1pass should use dynamic_loudness, got %+v", norm1)
	}

	norm2 := agent.sapCall("audio.setNormalize", map[string]any{
		"clipId": clipID, "mode": "2pass", "targetLevel": float64(-20),
	})
	if norm2["mltService"] != "loudness" {
		t.Fatalf("audio.setNormalize 2pass should use loudness, got %+v", norm2)
	}

	fade := agent.sapCall("audio.setFadeInOut", map[string]any{
		"clipId":        clipID,
		"fadeInFrames":  float64(fadeIn),
		"fadeOutFrames": float64(fadeOut),
	})
	fadeInInfo, _ := fade["fadeIn"].(map[string]any)
	fadeOutInfo, _ := fade["fadeOut"].(map[string]any)
	if fadeInInfo == nil || fadeInInfo["mltService"] != "volume" {
		t.Fatalf("audio.setFadeInOut fadeIn should be volume, got %+v", fade)
	}
	if fadeOutInfo == nil || fadeOutInfo["mltService"] != "volume" {
		t.Fatalf("audio.setFadeInOut fadeOut should be volume, got %+v", fade)
	}

	auto := agent.sapCall("audio.setAutoFade", map[string]any{"clipId": clipID, "enabled": true})
	if auto["mltService"] != "autofade" {
		t.Fatalf("audio.setAutoFade should add autofade, got %+v", auto)
	}

	// Persist while autofade is present, then disable via the same convenience API
	// (removes autofade filters through filter.remove under the hood).
	agent.sapCall("project.save", map[string]any{})
	projectXML, err := os.ReadFile(filepath.Join(proj.RootDir, "project.mlt"))
	if err != nil {
		t.Fatalf("read serialized project: %v", err)
	}
	xml := string(projectXML)

	mustContain := []string{
		`<property name="mlt_service">panner</property>`,
		`<property name="channel">0</property>`,
		`<property name="start">0</property>`,
		`<property name="split">0.25</property>`,
		`<property name="channel">-1</property>`,
		`<property name="split">0.75</property>`,
		`<property name="mlt_service">dynamic_loudness</property>`,
		`<property name="target_loudness">-18.0</property>`,
		`<property name="mlt_service">loudness</property>`,
		`<property name="program">-20.0</property>`,
		`<property name="mlt_service">volume</property>`,
		// fade-in envelope: -60 @ 0, 0 @ fadeIn-1
		fmt.Sprintf(`0=-60;%d=0`, fadeIn-1),
		// fade-out envelope: 0 @ (clipLen-fadeOut), -60 @ (clipLen-1)
		fmt.Sprintf(`%d=0;%d=-60`, clipLen-fadeOut, clipLen-1),
		`<property name="mlt_service">autofade</property>`,
		`<property name="fade_duration">500</property>`,
	}
	for _, want := range mustContain {
		if !strings.Contains(xml, want) {
			t.Fatalf("serialized MLT missing %q, XML=%s", want, xml)
		}
	}

	disabled := agent.sapCall("audio.setAutoFade", map[string]any{"clipId": clipID, "enabled": false})
	if disabled["enabled"] != false {
		t.Fatalf("audio.setAutoFade(false) should report enabled=false, got %+v", disabled)
	}
	if removed, ok := disabled["removed"].(float64); !ok || removed < 1 {
		t.Fatalf("audio.setAutoFade(false) should remove at least one autofade filter, got %+v", disabled)
	}
	agent.sapCall("project.save", map[string]any{})
	projectXML, err = os.ReadFile(filepath.Join(proj.RootDir, "project.mlt"))
	if err != nil {
		t.Fatalf("read serialized project after autofade disable: %v", err)
	}
	if strings.Contains(string(projectXML), `<property name="mlt_service">autofade</property>`) {
		t.Fatalf("autofade should be gone after setAutoFade(false), XML=%s", projectXML)
	}

	// Remaining non-autofade filters must still be present after disable.
	mustStillContain := []string{
		`<property name="mlt_service">panner</property>`,
		`<property name="mlt_service">dynamic_loudness</property>`,
		`<property name="mlt_service">loudness</property>`,
		`<property name="mlt_service">volume</property>`,
	}
	for _, want := range mustStillContain {
		if !strings.Contains(string(projectXML), want) {
			t.Fatalf("after autofade disable missing %q, XML=%s", want, projectXML)
		}
	}
}
