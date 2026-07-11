package mcpadapter_test

// TestMCPAdapter_SapCallTool_RealExport_EndToEnd is the concrete proof
// requested by 11-e2e-scenario-tests.md Phase A: an MCP client (exactly the
// transport/tool shape a real agent uses -- SSE, `sap.call`) drives the
// daemon -> sapproxy -> real sap-rust binary chain through a realistic
// creative sequence (playlist.append a real source, add a track, generate a
// title, attach a clip, export) and asserts a REAL playable video file
// exists on disk afterward, inspected with a real `ffprobe` run against the
// actual bytes `melt` produced -- not anything the server reports about
// itself.
//
// Since daemon.Launch (as of this pass) resolves the real Project.RootDir
// and forwards it to the child as SNAPSHOT_PROJECT_ROOT, and sap-rust's
// main.rs picks MltBackend over MockBackend whenever that env var is
// present, the real sap-rust process launched by this test really does
// shell out to `melt` and produce a real MP4 -- there is no test-only
// special-casing here, this is the exact same launch path
// `snapshotd serve` uses in production.

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"image"
	_ "image/jpeg"
	_ "image/png"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"testing"
	"time"

	mcpclient "github.com/mark3labs/mcp-go/client"
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"
)

// generateTestSource shells out to a real `ffmpeg` (lavfi testsrc + sine
// tone) to produce a short, real H.264/AAC source clip -- mirrors
// sap-rust/tests/mlt_export_integration.rs's `generate_test_source` helper
// exactly, so both the direct-JSON-RPC test and this MCP test exercise the
// identical real-media pipeline.
func generateTestSource(t *testing.T, dir string, durationSecs int) string {
	t.Helper()
	path := filepath.Join(dir, "source.mp4")
	cmd := exec.Command("ffmpeg",
		"-y",
		"-f", "lavfi",
		"-i", "testsrc=size=640x360:rate=30:duration="+strconv.Itoa(durationSecs),
		"-f", "lavfi",
		"-i", "sine=frequency=440:duration="+strconv.Itoa(durationSecs),
		"-c:v", "libx264",
		"-c:a", "aac",
		"-shortest",
		"-loglevel", "error",
		path,
	)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("ffmpeg failed to generate test source: %v\n%s", err, out)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("ffmpeg reported success but %s doesn't exist: %v", path, err)
	}
	return path
}

func generateTestOverlay(t *testing.T, dir string, durationSecs int) string {
	t.Helper()
	path := filepath.Join(dir, "overlay.mp4")
	cmd := exec.Command("ffmpeg",
		"-y",
		"-f", "lavfi",
		"-i", "color=c=0xFF1493:size=320x180:rate=30:duration="+strconv.Itoa(durationSecs),
		"-c:v", "libx264",
		"-pix_fmt", "yuv420p",
		"-loglevel", "error",
		path,
	)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("ffmpeg failed to generate test overlay: %v\n%s", err, out)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("ffmpeg reported success but %s doesn't exist: %v", path, err)
	}
	return path
}

// ffprobeStreamsAndDuration shells out to a real `ffprobe` and returns
// (hasVideoH264, hasAudio, durationSeconds).
func ffprobeStreamsAndDuration(t *testing.T, path string) (bool, bool, float64) {
	t.Helper()
	out, err := exec.Command("ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json", path).Output()
	if err != nil {
		t.Fatalf("ffprobe failed on %s: %v", path, err)
	}
	var probe struct {
		Streams []struct {
			CodecType string `json:"codec_type"`
			CodecName string `json:"codec_name"`
		} `json:"streams"`
		Format struct {
			Duration string `json:"duration"`
		} `json:"format"`
	}
	if err := json.Unmarshal(out, &probe); err != nil {
		t.Fatalf("unmarshal ffprobe JSON: %v (raw: %s)", err, out)
	}
	hasVideo, hasAudio := false, false
	for _, s := range probe.Streams {
		if s.CodecType == "video" && s.CodecName == "h264" {
			hasVideo = true
		}
		if s.CodecType == "audio" {
			hasAudio = true
		}
	}
	duration, err := strconv.ParseFloat(probe.Format.Duration, 64)
	if err != nil {
		t.Fatalf("parse ffprobe duration %q: %v", probe.Format.Duration, err)
	}
	return hasVideo, hasAudio, duration
}

type realFrame struct {
	image image.Image
}

func decodeRealFrame(t *testing.T, result map[string]any) realFrame {
	t.Helper()
	data, ok := result["data"].(string)
	if !ok || data == "" {
		t.Fatalf("expected non-empty base64 frame data, got %+v", result)
	}
	raw, err := base64.StdEncoding.DecodeString(data)
	if err != nil {
		t.Fatalf("decode playback frame base64: %v", err)
	}
	frame, _, err := image.Decode(bytes.NewReader(raw))
	if err != nil {
		t.Fatalf("decode playback frame image: %v", err)
	}
	if got := frame.Bounds().Size(); got.X != 1280 || got.Y != 720 {
		t.Fatalf("expected 1280x720 playback frame, got %v", got)
	}
	return realFrame{image: frame}
}

func (f realFrame) fracMatching(x0, y0, x1, y1 int, pred func(r, g, b uint8) bool) float64 {
	hit, total := 0, 0
	for y := y0; y < y1; y++ {
		for x := x0; x < x1; x++ {
			r, g, b, _ := f.image.At(x, y).RGBA()
			if pred(uint8(r>>8), uint8(g>>8), uint8(b>>8)) {
				hit++
			}
			total++
		}
	}
	return float64(hit) / float64(total)
}

func (f realFrame) cornerMeanAbsDiff(other realFrame, size int) float64 {
	bounds := f.image.Bounds()
	regions := [][4]int{
		{0, 0, size, size},
		{bounds.Max.X - size, 0, bounds.Max.X, size},
		{0, bounds.Max.Y - size, size, bounds.Max.Y},
		{bounds.Max.X - size, bounds.Max.Y - size, bounds.Max.X, bounds.Max.Y},
	}
	var total float64
	var pixels int
	for _, region := range regions {
		for y := region[1]; y < region[3]; y++ {
			for x := region[0]; x < region[2]; x++ {
				r1, g1, b1, _ := f.image.At(x, y).RGBA()
				r2, g2, b2, _ := other.image.At(x, y).RGBA()
				total += absInt(int(r1>>8) - int(r2>>8))
				total += absInt(int(g1>>8) - int(g2>>8))
				total += absInt(int(b1>>8) - int(b2>>8))
				pixels++
			}
		}
	}
	return total / float64(pixels*3)
}

func absInt(value int) float64 {
	if value < 0 {
		return float64(-value)
	}
	return float64(value)
}

func TestMCPAdapter_SapCallTool_RealExport_EndToEnd(t *testing.T) {
	binPath := realSapRustBinary(t)
	if _, err := exec.LookPath("ffmpeg"); err != nil {
		t.Skip("ffmpeg not on PATH; required to generate the synthetic test source")
	}
	if _, err := exec.LookPath("ffprobe"); err != nil {
		t.Skip("ffprobe not on PATH; required to verify the exported file")
	}

	workdir := t.TempDir()
	source := generateTestSource(t, workdir, 9)
	overlay := generateTestOverlay(t, workdir, 2)

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

	ctx, cancel := context.WithTimeout(context.Background(), 180*time.Second)
	defer cancel()

	proj, err := d.CreateProject(ctx, daemon.CreateProjectParams{Name: "mcp-export-e2e"})
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

	mcpServer := mcpadapter.New(d)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	sapCall := func(method string, params map[string]any) map[string]any {
		t.Helper()
		req := mcp.CallToolRequest{}
		req.Params.Name = "sap.call"
		req.Params.Arguments = map[string]any{"method": method, "params": params}
		res, err := c.CallTool(ctx, req)
		if err != nil {
			t.Fatalf("sap.call(%s): %v", method, err)
		}
		if res.IsError {
			t.Fatalf("sap.call(%s) returned an error result: %s", method, toolResultText(res))
		}
		return decodeToolResultJSON(t, res)
	}

	sel := sapCall("project.select", map[string]any{"projectId": proj.ID})
	if sel["projectId"] != proj.ID {
		t.Fatalf("expected real ProjectState.projectId == %s, got %+v", proj.ID, sel)
	}

	probe := sapCall("file.probe", map[string]any{"path": source})
	if probe["path"] != source || probe["codec"] != "h264" ||
		probe["durationFrames"] != float64(270) {
		t.Fatalf("expected real source probe metadata for 9s H.264 input, got %+v", probe)
	}

	appended := sapCall("playlist.append", map[string]any{"source": map[string]any{"path": source}})
	if appended["index"] != float64(0) {
		t.Fatalf("expected source playlist index 0, got %+v", appended)
	}
	if appended["durationFrames"] != float64(270) {
		t.Fatalf("expected 9s source to probe as 270 frames, got %+v", appended)
	}

	overlayEntry := sapCall("playlist.append", map[string]any{"source": map[string]any{"path": overlay}})
	if overlayEntry["index"] != float64(1) || overlayEntry["durationFrames"] != float64(60) {
		t.Fatalf("expected 2s overlay playlist entry at index 1, got %+v", overlayEntry)
	}

	sapCall("edit.addTrack", map[string]any{"kind": "video"})

	title := sapCall("generator.createTitle", map[string]any{"mode": "simple", "text": "MCP E2E Highlights"})
	titleIndex, _ := title["index"].(float64)
	titleDurationFrames, _ := title["durationFrames"].(float64)
	if titleDurationFrames <= 0 {
		t.Fatalf("expected a positive durationFrames from generator.createTitle, got %+v", title)
	}

	// V1 is a sequential creative reel: title, then three source clips.
	// Multi-track video compositing is exercised separately below on V2.
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": titleIndex},
	})
	seg1 := sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": float64(0)},
	})
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": float64(0)},
	})
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": float64(0)},
	})

	trim := func(clipIndex, inFrame, outFrame int) {
		t.Helper()
		sapCall("edit.trimClipIn", map[string]any{
			"trackIndex": float64(0),
			"clipIndex":  float64(clipIndex),
			"newFrame":   float64(inFrame),
		})
		sapCall("edit.trimClipOut", map[string]any{
			"trackIndex": float64(0),
			"clipIndex":  float64(clipIndex),
			"newFrame":   float64(outFrame),
		})
	}
	trim(1, 0, 44)
	trim(2, 90, 134)
	trim(3, 200, 244)

	sapCall("transitions.addCrossfade", map[string]any{
		"trackIndex":     float64(0),
		"betweenClips":   []int{1, 2},
		"durationFrames": float64(15),
	})
	sapCall("transitions.addCrossfade", map[string]any{
		"trackIndex":     float64(0),
		"betweenClips":   []int{2, 3},
		"durationFrames": float64(15),
	})

	seg1ClipID, _ := seg1["clipId"].(string)
	zoom := sapCall("filter.add", map[string]any{
		"clipId":     seg1ClipID,
		"mltService": "affine",
		"properties": map[string]any{"transition.distort": 1},
	})
	zoomIndex, _ := zoom["filterIndex"].(float64)
	for _, keyframe := range []map[string]any{
		{"position": 0, "value": "0% 0% 100% 100% 1"},
		{"position": 44, "value": "-20% -20% 140% 140% 1"},
	} {
		sapCall("filter.addKeyframe", map[string]any{
			"clipId":        seg1ClipID,
			"filterIndex":   zoomIndex,
			"property":      "transition.rect",
			"position":      float64(keyframe["position"].(int)),
			"value":         keyframe["value"],
			"interpolation": "linear",
		})
	}

	// V2 has a deterministic 190-frame lead and 5-frame trail. The real
	// MltBackend serializes these as transparent MLT spacer producers, which
	// positions the 60-frame overlay at the same timeline coordinates as the
	// direct Rust doc-11 test.
	sapCall("edit.addTrack", map[string]any{"kind": "video"})
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(1),
		"source":     map[string]any{"blank": float64(190)},
	})
	overlayClip := sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(1),
		"source":     map[string]any{"playlistIndex": float64(1)},
	})
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(1),
		"source":     map[string]any{"blank": float64(5)},
	})
	overlayClipID, _ := overlayClip["clipId"].(string)

	slide := sapCall("filter.add", map[string]any{
		"clipId":     overlayClipID,
		"mltService": "affine",
		"properties": map[string]any{"transition.distort": 1, "transition.fill": 1},
	})
	slideIndex, _ := slide["filterIndex"].(float64)
	for _, keyframe := range []struct {
		position float64
		value    string
	}{
		{0, "120% -20% 30% 30% 1"},
		{10, "65% 5% 30% 30% 1"},
		{59, "65% 5% 30% 30% 1"},
	} {
		sapCall("filter.addKeyframe", map[string]any{
			"clipId":        overlayClipID,
			"filterIndex":   slideIndex,
			"property":      "transition.rect",
			"position":      keyframe.position,
			"value":         keyframe.value,
			"interpolation": "linear",
		})
	}
	brightness := sapCall("filter.add", map[string]any{
		"clipId":     overlayClipID,
		"mltService": "brightness",
		"properties": map[string]any{},
	})
	brightnessIndex, _ := brightness["filterIndex"].(float64)
	for _, keyframe := range []struct {
		position float64
		value    float64
	}{
		{0, 1}, {40, 1}, {59, 0},
	} {
		sapCall("filter.addKeyframe", map[string]any{
			"clipId":        overlayClipID,
			"filterIndex":   brightnessIndex,
			"property":      "level",
			"position":      keyframe.position,
			"value":         keyframe.value,
			"interpolation": "linear",
		})
	}

	sapCall("subtitles.addTrack", map[string]any{})
	sapCall("subtitles.appendItem", map[string]any{
		"trackIndex": 0, "startFrame": 60, "endFrame": 90, "text": "Highlight One",
	})
	sapCall("subtitles.appendItem", map[string]any{
		"trackIndex": 0, "startFrame": 200, "endFrame": 230, "text": "Highlight Two",
	})

	exportDir := filepath.Join(workdir, "out")
	if err := os.MkdirAll(exportDir, 0o755); err != nil {
		t.Fatalf("mkdir export dir: %v", err)
	}
	outputPath := filepath.Join(exportDir, "mcp-e2e.mp4")
	exportRes := sapCall("file.export", map[string]any{
		"outputPath": outputPath,
		"codec":      "libx264",
		"container":  "mp4",
	})
	jobID, _ := exportRes["jobId"].(string)
	if jobID == "" {
		t.Fatalf("expected a real jobId from file.export, got %+v", exportRes)
	}

	// jobs.get polling, over MCP, until the real background melt subprocess
	// finishes -- per doc 01's async-job convention and doc 11 Phase A's
	// "jobs.get polled/notified until complete" step.
	status := "running"
	var lastJob map[string]any
	deadline := time.Now().Add(45 * time.Second)
	for time.Now().Before(deadline) {
		lastJob = sapCall("jobs.get", map[string]any{"jobId": jobID})
		status, _ = lastJob["status"].(string)
		if status != "running" {
			break
		}
		time.Sleep(300 * time.Millisecond)
	}
	if status != "done" {
		t.Fatalf("export job should finish successfully via MCP polling, last status: %+v", lastJob)
	}

	// The real proof, reached entirely through MCP tool calls: an actual
	// video file on disk, verified with a real ffprobe run.
	if _, err := os.Stat(outputPath); err != nil {
		t.Fatalf("exported file should exist at %s: %v", outputPath, err)
	}
	hasVideo, hasAudio, duration := ffprobeStreamsAndDuration(t, outputPath)
	if !hasVideo {
		t.Fatalf("exported file should have an h264 video stream")
	}
	if !hasAudio {
		t.Fatalf("exported file should have an audio stream")
	}
	expectedFrames := titleDurationFrames + (3*45 - 2*15)
	expectedSecs := expectedFrames / 30
	if diff := duration - expectedSecs; diff > 0.5 || diff < -0.5 {
		t.Fatalf("exported duration %.3fs should be close to expected %.3fs (%d frames: title %.0ff + three 45-frame clips - two 15-frame crossfades)", duration, expectedSecs, int(expectedFrames), titleDurationFrames)
	}

	grab := func(frame int) realFrame {
		t.Helper()
		return decodeRealFrame(t, sapCall("playback.getFrame", map[string]any{
			"frame": float64(frame), "format": "png",
		}))
	}

	// The claims below inspect decoded pixels from real melt frame renders,
	// not merely successful RPC responses.
	zoomEarly := grab(152)
	zoomLate := grab(178)
	if diff := zoomEarly.cornerMeanAbsDiff(zoomLate, 80); diff <= 25 {
		t.Fatalf("zoom keyframes should visibly shift the frame corners, mean RGB diff %.2f <= 25", diff)
	}

	titleIn := grab(50)
	titleOut := grab(160)
	isNearWhite := func(r, g, b uint8) bool { return r > 200 && g > 200 && b > 200 }
	titleInFrac := titleIn.fracMatching(320, 288, 960, 432, isNearWhite)
	titleOutFrac := titleOut.fracMatching(320, 288, 960, 432, isNearWhite)
	if titleInFrac <= titleOutFrac+0.01 {
		t.Fatalf("title should be visibly present at frame 50: in-window %.4f, out-of-window %.4f", titleInFrac, titleOutFrac)
	}

	overlayBefore := grab(100)
	overlayDuring := grab(210)
	overlayAfter := grab(253)
	isDeepPink := func(r, g, b uint8) bool { return r > 200 && g < 90 && b > 90 && b < 200 }
	overlayRect := func(frame realFrame) float64 {
		return frame.fracMatching(832, 36, 1216, 252, isDeepPink)
	}
	beforeFrac, duringFrac, afterFrac := overlayRect(overlayBefore), overlayRect(overlayDuring), overlayRect(overlayAfter)
	if duringFrac <= 0.5 || beforeFrac >= 0.05 || afterFrac >= 0.05 {
		t.Fatalf("overlay timing/placement mismatch: deep-pink fraction before=%.4f during=%.4f after=%.4f", beforeFrac, duringFrac, afterFrac)
	}

	subtitleIn := grab(75)
	subtitleOut := grab(165)
	isSubtitleWhite := func(r, g, b uint8) bool { return r > 220 && g > 220 && b > 220 }
	subtitleInFrac := subtitleIn.fracMatching(320, 576, 960, 691, isSubtitleWhite)
	subtitleOutFrac := subtitleOut.fracMatching(320, 576, 960, 691, isSubtitleWhite)
	if subtitleInFrac <= subtitleOutFrac+0.01 {
		t.Fatalf("subtitle burn-in should be visibly present at frame 75: in-window %.4f, out-of-window %.4f", subtitleInFrac, subtitleOutFrac)
	}

	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}

	t.Logf("MCP end-to-end export succeeded: %s (video=%v audio=%v duration=%.3fs)", outputPath, hasVideo, hasAudio, duration)
}
