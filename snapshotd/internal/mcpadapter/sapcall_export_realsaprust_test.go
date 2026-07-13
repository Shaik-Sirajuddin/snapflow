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
	"image/png"
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
	// The real Qt binary's project profile for a brand-new project (no
	// prior QSettings/Shotcut.conf under its HOME) is Shotcut's own
	// built-in default ("atsc_1080p_2997", 1920x1080) -- confirmed live,
	// not assumed -- until something in the edit sequence sets a
	// different one explicitly. This test never does, so 1920x1080 is the
	// deterministic value for a genuinely fresh environment. It used to
	// read 1280x720 here because every prior run of this test (and every
	// other live-Qt harness in this repo) happened to reuse a real,
	// long-lived $HOME that had a leftover Shotcut.conf-stored profile
	// preference from years of manual/agent use -- procmgr.go's
	// per-project isolated HOME (added for a real, separate FilesDock
	// startup-latency/QSettings-corruption bug) exposed that this was
	// never actually a guaranteed value, just an incidental one.
	if got := frame.Bounds().Size(); got.X != 1920 || got.Y != 1080 {
		t.Fatalf("expected 1920x1080 playback frame (fresh-HOME default profile), got %v", got)
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

// dumpRealFramePNG writes a decoded frame to disk under dir/name.png for
// manual visual inspection; only used when SNAPSHOT_DEBUG_FRAME_DUMP is set.
func dumpRealFramePNG(t *testing.T, dir, name string, f realFrame) {
	t.Helper()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir debug dump dir: %v", err)
	}
	out, err := os.Create(filepath.Join(dir, name+".png"))
	if err != nil {
		t.Fatalf("create debug dump file: %v", err)
	}
	defer out.Close()
	if err := png.Encode(out, f.image); err != nil {
		t.Fatalf("encode debug dump PNG: %v", err)
	}
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

	// file.probe (media_tools.rs's probe_media) shells out to real
	// ffprobe directly and reports the SOURCE's own native nb_frames --
	// stateless, independent of any live MLT project profile, so a 9s
	// 30fps-encoded source genuinely reports 270 here. playlist.append,
	// by contrast, opens a real Mlt::Producer against the live project's
	// profile (sap_playlist_append in sap_ffi.cpp), which this
	// environment's real default profile fixes at exactly 25fps
	// (confirmed via ffprobe on a real export) until something changes
	// it -- so the SAME file reports a different (profile-relative)
	// frame count once it's actually added to the project. Both numbers
	// are correct for what each call actually measures; they are not
	// expected to agree.
	const fps = 25
	probe := sapCall("file.probe", map[string]any{"path": source})
	if probe["path"] != source || probe["codec"] != "h264" ||
		probe["durationFrames"] != float64(270) {
		t.Fatalf("expected real source probe metadata for 9s H.264 input, got %+v", probe)
	}

	appended := sapCall("playlist.append", map[string]any{"source": map[string]any{"path": source}})
	if appended["index"] != float64(0) {
		t.Fatalf("expected source playlist index 0, got %+v", appended)
	}
	if appended["durationFrames"] != float64(9*fps) {
		t.Fatalf("expected 9s source to probe as %d frames, got %+v", 9*fps, appended)
	}

	overlayEntry := sapCall("playlist.append", map[string]any{"source": map[string]any{"path": overlay}})
	if overlayEntry["index"] != float64(1) || overlayEntry["durationFrames"] != float64(2*fps) {
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
		// Real MultitrackModel::trimClipOutValid checks
		// (frame_out-delta) against the clip's *current* reported
		// length, which a prior in-trim already narrows -- trimming
		// out first (while length still reflects the full untrimmed
		// producer) then trimming in avoids a spurious "out of range"
		// rejection that the reverse order hits (confirmed via manual
		// probe against the real binary).
		sapCall("edit.trimClipOut", map[string]any{
			"trackIndex": float64(0),
			"clipIndex":  float64(clipIndex),
			"newFrame":   float64(outFrame),
			"ripple":     true,
		})
		sapCall("edit.trimClipIn", map[string]any{
			"trackIndex": float64(0),
			"clipIndex":  float64(clipIndex),
			"newFrame":   float64(inFrame),
			"ripple":     true,
		})
	}
	// Three ~1.6s (40-frame, 25fps) windows sliced from the same 225-frame
	// source, spread across it (early/mid/late) -- 45/30/1.5s numbers from
	// an earlier 30fps-source assumption no longer fit inside the real
	// 225-frame budget (200-244 alone exceeds it), so this schedule is a
	// from-scratch 25fps-native one, not a mechanical rescale.
	const segLen = 40
	trim(1, 0, segLen-1)
	trim(2, 90, 90+segLen-1)
	trim(3, 180, 180+segLen-1)

	const crossfadeLen = 15
	sapCall("transitions.addCrossfade", map[string]any{
		"trackIndex":     float64(0),
		"betweenClips":   []int{1, 2},
		"durationFrames": float64(crossfadeLen),
	})
	// AddTransitionCommand inserts the transition as its own raw playlist
	// entry between clip1 and clip2, shifting every later clip's index up
	// by one (clip2 was 2, is now 3; clip3 was 3, is now 4) -- confirmed
	// via manual probe against the real binary.
	sapCall("transitions.addCrossfade", map[string]any{
		"trackIndex":     float64(0),
		"betweenClips":   []int{3, 4},
		"durationFrames": float64(crossfadeLen),
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
		{"position": segLen - 1, "value": "-20% -20% 140% 140% 1"},
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

	// V2 has a deterministic 130-frame lead and 10-frame trail, sized so
	// V2's total (130 + the 50-frame overlay + 10) exactly matches V1's
	// own total length below, avoiding any ambiguity about which track's
	// length governs the exported duration. There is no "blank"/spacer
	// source form in the real spec (01-jsonrpc-spec.md's edit.appendClip
	// union is strictly {playlistIndex}|{path}|{xml}) -- a real spacer is
	// instead a fully transparent (#00000000) generator.createColor clip,
	// appended then narrowed with edit.trimClipOut to the target length,
	// which is real, undoable, and already spec'd (this schedule is
	// bespoke to this test's real 25fps profile, see the segLen/
	// crossfadeLen note above).
	//
	// Confirmed via manual probe: a freshly added video track is always
	// inserted at real trackIndex 0 (topmost), shifting every existing
	// video track's index up by one -- V1 (all its own appends/trims/
	// crossfades/filters above are already done) shifts from trackIndex
	// 0 to trackIndex 1 the instant this second edit.addTrack runs. V2
	// (this new track) is trackIndex 0 from here on, which also happens
	// to be exactly the topmost/compositing-on-top position it needs for
	// the overlay to actually appear over V1 in the export.
	const overlayLead = 130
	const overlayTrail = 10
	sapCall("edit.addTrack", map[string]any{"kind": "video"})
	spacer := sapCall("generator.createColor", map[string]any{"hexColor": "#00000000"})
	spacerIndex, _ := spacer["index"].(float64)
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": spacerIndex},
	})
	sapCall("edit.trimClipOut", map[string]any{
		"trackIndex": float64(0),
		"clipIndex":  float64(0),
		"newFrame":   float64(overlayLead - 1),
	})
	overlayClip := sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": float64(1)},
	})
	sapCall("edit.appendClip", map[string]any{
		"trackIndex": float64(0),
		"source":     map[string]any{"playlistIndex": spacerIndex},
	})
	sapCall("edit.trimClipOut", map[string]any{
		"trackIndex": float64(0),
		"clipIndex":  float64(2),
		"newFrame":   float64(overlayTrail - 1),
	})
	overlayClipID, _ := overlayClip["clipId"].(string)

	slide := sapCall("filter.add", map[string]any{
		"clipId":     overlayClipID,
		"mltService": "affine",
		"properties": map[string]any{"transition.distort": 1, "transition.fill": 1},
	})
	slideIndex, _ := slide["filterIndex"].(float64)
	// overlay's own real (profile-relative) length is 2*fps == 50 frames
	// (2s @ 25fps) -- local keyframe positions must stay within [0, 49],
	// unlike the original 60-frame-native (30fps-source-assumed) schedule.
	const overlayLocalLen = 2 * fps
	for _, keyframe := range []struct {
		position float64
		value    string
	}{
		{0, "120% -20% 30% 30% 1"},
		{overlayLocalLen * 0.16, "65% 5% 30% 30% 1"},
		{overlayLocalLen - 1, "65% 5% 30% 30% 1"},
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
		{0, 1}, {overlayLocalLen * 0.66, 1}, {overlayLocalLen - 1, 0},
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
		// Was startFrame/endFrame 200/230 under the old 30fps-source
		// assumption -- V1's real total is now only 190 frames (see
		// expectedFrames below), so that window would fall entirely past
		// the end of the exported video. Relocated inside clip3's span
		// instead, still clear of both grab(165) (must stay
		// subtitle-free) and "Highlight One" above.
		"trackIndex": 0, "startFrame": 100, "endFrame": 120, "text": "Highlight Two",
	})
	// subtitles.appendItem alone only edits SubtitlesModel's cue data; it
	// never makes cues visible in rendered/exported frames on its own
	// (mirrors the real app: SubtitlesDock has a dedicated "Burn In
	// Subtitles on Output" action that attaches a real MLT "subtitle"
	// filter to the timeline output). subtitles.burnIn is that primitive.
	sapCall("subtitles.burnIn", map[string]any{"trackIndex": 0})

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
	// V1 = title + 3 segLen-frame clips joined by 2 crossfadeLen-frame
	// crossfades (each crossfade shortens the combined length by its own
	// duration, since the two clips play blended, not back-to-back).
	expectedFrames := titleDurationFrames + float64(3*segLen-2*crossfadeLen)
	expectedSecs := expectedFrames / fps
	if diff := duration - expectedSecs; diff > 0.5 || diff < -0.5 {
		t.Fatalf("exported duration %.3fs should be close to expected %.3fs (%d frames: title %.0ff + three %d-frame clips - two %d-frame crossfades)", duration, expectedSecs, int(expectedFrames), titleDurationFrames, segLen, crossfadeLen)
	}

	grab := func(frame int) realFrame {
		t.Helper()
		return decodeRealFrame(t, sapCall("playback.getFrame", map[string]any{
			"frame": float64(frame), "format": "png",
		}))
	}

	// The claims below inspect decoded pixels from real melt frame renders,
	// not merely successful RPC responses.
	// clip1 occupies absolute frames [100,139] (title is 100 frames, clip1
	// is segLen=40 long); 105/135 sample near its local start/end.
	zoomEarly := grab(105)
	zoomLate := grab(135)
	if diff := zoomEarly.cornerMeanAbsDiff(zoomLate, 80); diff <= 25 {
		t.Fatalf("zoom keyframes should visibly shift the frame corners, mean RGB diff %.2f <= 25", diff)
	}

	titleIn := grab(50)
	titleOut := grab(150)
	if dir := os.Getenv("SNAPSHOT_DEBUG_FRAME_DUMP"); dir != "" {
		dumpRealFramePNG(t, dir, "title-in-50", titleIn)
		dumpRealFramePNG(t, dir, "title-out-150", titleOut)
	}
	isNearWhite := func(r, g, b uint8) bool { return r > 200 && g > 200 && b > 200 }
	// Sample windows below are relative-position rects (25%-75% width,
	// etc.) scaled for the real 1920x1080 default profile (1.5x the
	// original 1280x720 pixel coordinates these were authored against --
	// see decodeRealFrame's doc comment on why the resolution changed).
	titleInFrac := titleIn.fracMatching(480, 432, 1440, 648, isNearWhite)
	titleOutFrac := titleOut.fracMatching(480, 432, 1440, 648, isNearWhite)
	if titleInFrac <= titleOutFrac+0.01 {
		t.Fatalf("title should be visibly present at frame 50: in-window %.4f, out-of-window %.4f", titleInFrac, titleOutFrac)
	}

	// Overlay (V2) is visible for absolute frames [overlayLead,
	// overlayLead+overlayLocalLen) = [130,179].
	overlayBefore := grab(110)
	overlayDuring := grab(155)
	overlayAfter := grab(185)
	isDeepPink := func(r, g, b uint8) bool { return r > 200 && g < 90 && b > 90 && b < 200 }
	overlayRect := func(frame realFrame) float64 {
		return frame.fracMatching(1248, 54, 1824, 378, isDeepPink)
	}
	beforeFrac, duringFrac, afterFrac := overlayRect(overlayBefore), overlayRect(overlayDuring), overlayRect(overlayAfter)
	if duringFrac <= 0.5 || beforeFrac >= 0.05 || afterFrac >= 0.05 {
		t.Fatalf("overlay timing/placement mismatch: deep-pink fraction before=%.4f during=%.4f after=%.4f", beforeFrac, duringFrac, afterFrac)
	}

	subtitleIn := grab(75)
	subtitleOut := grab(165)
	isSubtitleWhite := func(r, g, b uint8) bool { return r > 220 && g > 220 && b > 220 }
	subtitleInFrac := subtitleIn.fracMatching(480, 864, 1440, 1037, isSubtitleWhite)
	subtitleOutFrac := subtitleOut.fracMatching(480, 864, 1440, 1037, isSubtitleWhite)
	if subtitleInFrac <= subtitleOutFrac+0.01 {
		t.Fatalf("subtitle burn-in should be visibly present at frame 75: in-window %.4f, out-of-window %.4f", subtitleInFrac, subtitleOutFrac)
	}

	if err := d.CloseInstance(ctx, pi.ID); err != nil {
		t.Fatalf("close instance: %v", err)
	}

	t.Logf("MCP end-to-end export succeeded: %s (video=%v audio=%v duration=%.3fs)", outputPath, hasVideo, hasAudio, duration)
}
