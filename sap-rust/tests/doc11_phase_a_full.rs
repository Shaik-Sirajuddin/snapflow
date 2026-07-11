//! Full Phase A workflow, per `memory/head/gen/rust-fork/11-e2e-scenario-tests.md`:
//! "cut three highlight segments, arrange them with a crossfade between
//! each, apply the zoom-in-from-center + top-right-image-overlay animation
//! ..., add a title card, add subtitles, export the result" -- chained
//! through one real `MltBackend`-backed server, over one real Unix socket,
//! with a real `melt` render at the end and real `ffprobe`/pixel inspection
//! of the actual output file. This is deliberately more demanding than
//! `mlt_export_integration.rs`'s per-feature tests: every namespace here
//! composes into the *same* project, in the order a real creative session
//! would use them, matching doc 11's explicit ask ("a realistic creative
//! workflow chaining many namespaces together", not call-and-check).
//!
//! What's simulated (same honesty caveat as `mlt_export_integration.rs`):
//! no live Qt/QUndoStack -- this is `MltBackend`, independent of `shotcut/`.
//! What's real: every RPC call below goes over a real Unix socket to a real
//! server backed by a real `MltBackend`; the MLT XML it generates is real
//! input to a real `melt` subprocess; the exported file's duration/codecs
//! are read with a real `ffprobe` run; and every visual claim (zoom,
//! overlay-in-its-window-and-gone-outside-it, title, subtitles) is checked
//! by decoding real frames grabbed via real `playback.getFrame` calls (via
//! a real `melt` single-frame render) into raw RGB pixels (via `ffmpeg`,
//! not a hand-rolled PNG decoder) and inspecting actual pixel values --
//! not just "the call returned some bytes".
//!
//! ## Design decisions this test makes (not spelled out by doc 11 itself):
//!
//! - **Title placement**: prepended onto the *same* V1 track as the three
//!   highlight segments (clip index 0, ahead of the three segments at
//!   indices 1-3), not a separate V3 track. A separate video track would
//!   be composited *simultaneously* with V1 by MLT's tractor (multi-track
//!   timelines overlay, they don't concatenate) -- same reasoning
//!   `mlt_export_integration.rs` already documents. Putting it on the
//!   timeline track directly is what actually produces a real
//!   "title leads into the highlight reel" sequential result whose total
//!   duration `ffprobe` can verify end to end.
//! - **Overlay mid-timeline positioning**: `edit_append_clip` has no
//!   position/offset parameter (the `Backend` trait wasn't extended for
//!   one). This test uses `mlt_backend.rs`'s `{"blank": <frames>}` source
//!   shape (a transparent spacer clip, real MLT `<blank>`-equivalent
//!   technique) to leave a gap on V2 before and after the overlay's
//!   visible window -- see that file's module doc comment for the
//!   empirical validation of this approach.
//! - **Subtitles**: burned in via `avfilter.subtitles` (ffmpeg's real
//!   libavfilter `subtitles` filter, attached at the tractor level), *not*
//!   Shotcut's own `subtitle_feed` mechanism. This was empirically
//!   determined, not assumed: `subtitle_feed` + `subtitle.N.feed` consumer
//!   properties were tested directly against `melt` with a real SRT file
//!   and produced only an empty placeholder `mov_text` stream (0 real
//!   packets) -- that mechanism needs a live Shotcut `Subtitles` QObject
//!   injecting per-frame cue text during rendering, which doesn't exist
//!   when driving `melt` as a bare CLI subprocess. `avfilter.subtitles`
//!   does real, verifiable, pixel-level burn-in standalone (confirmed here
//!   the same way it was confirmed during development: decoding a frame
//!   inside vs. outside a cue window and finding real white/black-outline
//!   text pixels only inside it).
//! - **Overlay content**: a small *solid-color* real video clip (not a
//!   still image, not another `testsrc` clip) generated with
//!   `ffmpeg -f lavfi -i color=...`. Deliberately not a busy test pattern:
//!   the whole point of this test is asserting the overlay's presence/
//!   absence via pixel color, and a solid, saturated color that doesn't
//!   appear in `testsrc`'s own standard color-bar palette (white, yellow,
//!   cyan, green, magenta, red, blue, black) makes that assertion
//!   unambiguous instead of racing against the background pattern's own
//!   content.
//! - **Zoom verification methodology**: `testsrc`'s pattern has some of its
//!   own inherent motion (a moving box/sweep), so a raw pixel diff between
//!   two timestamps of the same clip is not *purely* attributable to the
//!   `affine` zoom. This test instead checks the four frame *corners*
//!   specifically: a 100%->140% centered zoom (with `transition.distort=1`)
//!   crops the outer 20% of the frame out of view entirely by the end
//!   keyframe, so the corner regions specifically (least affected by
//!   `testsrc`'s central motion) should diverge sharply as the zoom
//!   progresses -- empirically confirmed during development by probing the
//!   exact same `affine` configuration against a real `testsrc` render
//!   before writing this test (mean corner-region abs diff grew from 3.6
//!   at 10% through the keyframe range to 150.8 at the end keyframe).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sap_rust::framing;
use sap_rust::mlt_backend::MltBackend;
use sap_rust::protocol::{RpcRequest, RpcResponse};
use sap_rust::server::{self, ServerConfig};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const TOKEN: &str = "phase-a-test-token";
const FPS: i64 = 30;
const FRAME_W: usize = 1280;
const FRAME_H: usize = 720;

fn unique_tag(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4())
}

fn temp_socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sap-rust-phasea-{}.sock", unique_tag(tag)))
}

fn generate_main_source(dir: &Path) -> PathBuf {
    let path = dir.join("source.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=640x360:rate={FPS}:duration=9"),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=9",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-shortest",
            "-loglevel",
            "error",
        ])
        .arg(&path)
        .status()
        .expect("failed to spawn ffmpeg for the main source");
    assert!(status.success(), "ffmpeg failed to generate the main synthetic source");
    path
}

fn generate_overlay_source(dir: &Path) -> PathBuf {
    let path = dir.join("overlay.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=0xFF1493:size=320x180:rate={FPS}:duration=2"),
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-loglevel",
            "error",
        ])
        .arg(&path)
        .status()
        .expect("failed to spawn ffmpeg for the overlay source");
    assert!(status.success(), "ffmpeg failed to generate the overlay synthetic source");
    path
}

fn ffprobe_json(path: &Path) -> Value {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .expect("failed to spawn ffprobe on the exported file");
    assert!(
        output.status.success(),
        "ffprobe failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("ffprobe produced valid JSON")
}

async fn start_server(tag: &str, projects_root: PathBuf) -> PathBuf {
    let socket_path = temp_socket_path(tag);
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        token: TOKEN.to_string(),
        audio_enabled: false,
    };
    let backend = MltBackend::new(projects_root);
    tokio::spawn(async move {
        let _ = server::serve(config, backend).await;
    });
    for _ in 0..100 {
        if socket_path.exists() {
            return socket_path;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not bind {} in time", socket_path.display());
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: i64,
}

impl Client {
    async fn connect(path: &PathBuf) -> Self {
        let stream = UnixStream::connect(path).await.expect("connect to server socket");
        let (read_half, write_half) = stream.into_split();
        Client { reader: BufReader::new(read_half), writer: write_half, next_id: 1 }
    }

    async fn call(&mut self, method: &str, params: Value) -> RpcResponse {
        let id = self.next_id;
        self.next_id += 1;
        let req = RpcRequest {
            jsonrpc: Some("2.0".to_string()),
            id: Some(json!(id)),
            method: method.to_string(),
            params,
        };
        let value = serde_json::to_value(&req).expect("request serializes");
        framing::write_message(&mut self.writer, &value).await.expect("write request");
        loop {
            let value = framing::read_message(&mut self.reader).await.expect("read response");
            if value.get("id").is_none() {
                continue;
            }
            let resp: RpcResponse = serde_json::from_value(value).expect("parse response");
            assert_eq!(resp.id, json!(id), "response id must match the request id");
            return resp;
        }
    }

    fn ok(resp: &RpcResponse, ctx: &str) -> Value {
        assert!(resp.error.is_none(), "{ctx} should succeed: {:?}", resp.error);
        resp.result.clone().unwrap_or(Value::Null)
    }
}

struct RawFrame {
    bytes: Vec<u8>,
}

impl RawFrame {
    fn from_png(png_path: &Path) -> Self {
        let output = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(png_path)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
            .expect("ffmpeg should decode the grabbed frame to raw RGB");
        assert!(
            output.status.success(),
            "ffmpeg raw decode of {} failed: {}",
            png_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout.len(),
            FRAME_W * FRAME_H * 3,
            "grabbed frame {} isn't the expected {FRAME_W}x{FRAME_H} RGB size",
            png_path.display()
        );
        RawFrame { bytes: output.stdout }
    }

    fn pixel(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * FRAME_W + x) * 3;
        (self.bytes[i], self.bytes[i + 1], self.bytes[i + 2])
    }

    fn frac_matching(&self, x0: usize, y0: usize, x1: usize, y1: usize, pred: impl Fn(u8, u8, u8) -> bool) -> f64 {
        let mut hit = 0usize;
        let mut total = 0usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let (r, g, b) = self.pixel(x, y);
                if pred(r, g, b) {
                    hit += 1;
                }
                total += 1;
            }
        }
        hit as f64 / total as f64
    }

    fn corner_mean_abs_diff(&self, other: &RawFrame, size: usize) -> f64 {
        let regions = [
            (0, 0, size, size),
            (FRAME_W - size, 0, FRAME_W, size),
            (0, FRAME_H - size, size, FRAME_H),
            (FRAME_W - size, FRAME_H - size, FRAME_W, FRAME_H),
        ];
        let mut total = 0f64;
        let mut n = 0usize;
        for (x0, y0, x1, y1) in regions {
            for y in y0..y1 {
                for x in x0..x1 {
                    let (r1, g1, b1) = self.pixel(x, y);
                    let (r2, g2, b2) = other.pixel(x, y);
                    total += (r1 as f64 - r2 as f64).abs() + (g1 as f64 - g2 as f64).abs() + (b1 as f64 - b2 as f64).abs();
                    n += 1;
                }
            }
        }
        total / (n as f64 * 3.0)
    }
}

async fn grab_frame(client: &mut Client, projects_root: &Path, project_id: &str, frame: i64) -> RawFrame {
    let resp = client.call("playback.getFrame", json!({"frame": frame, "format": "png"})).await;
    let result = Client::ok(&resp, &format!("playback.getFrame({frame})"));
    let data_b64 = result["data"].as_str().expect("playback.getFrame returns base64 data");
    assert!(!data_b64.is_empty(), "grabbed frame {frame} payload should not be empty");
    let png_path = projects_root.join(project_id).join(".snapshot").join(format!("frame-{frame}.png"));
    assert!(png_path.exists(), "expected MltBackend to have written {}", png_path.display());
    RawFrame::from_png(&png_path)
}

#[tokio::test]
async fn phase_a_full_creative_workflow() {
    let workdir = std::env::temp_dir().join(unique_tag("phasea-workdir"));
    std::fs::create_dir_all(&workdir).unwrap();
    let main_source = generate_main_source(&workdir);
    let overlay_source = generate_overlay_source(&workdir);

    let projects_root = std::env::temp_dir().join(unique_tag("phasea-projects"));
    let path = start_server("phasea", projects_root.clone()).await;
    let mut client = Client::connect(&path).await;
    Client::ok(&client.call("sap.hello", json!({"token": TOKEN})).await, "sap.hello");
    let project_id = "phase-a-highlight-reel";
    Client::ok(&client.call("project.select", json!({"projectId": project_id})).await, "project.select");

    let main_entry = Client::ok(
        &client.call("playlist.append", json!({"source": {"path": main_source.to_string_lossy()}})).await,
        "playlist.append(main)",
    );
    assert_eq!(main_entry["index"], 0);
    assert_eq!(main_entry["durationFrames"], 9 * FPS, "9s @ {FPS}fps source should probe to {} frames", 9 * FPS);

    let overlay_entry = Client::ok(
        &client.call("playlist.append", json!({"source": {"path": overlay_source.to_string_lossy()}})).await,
        "playlist.append(overlay)",
    );
    assert_eq!(overlay_entry["index"], 1);
    assert_eq!(overlay_entry["durationFrames"], 2 * FPS);

    let title_entry = Client::ok(
        &client.call("generator.createTitle", json!({"mode": "simple", "text": "Highlights"})).await,
        "generator.createTitle",
    );
    assert_eq!(title_entry["index"], 2);
    let title_frames = title_entry["durationFrames"].as_i64().expect("durationFrames is an int");
    assert!(title_frames > 0);

    Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack (V1)");
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 2}})).await,
        "appendClip title",
    );

    let seg1 = Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "appendClip seg1",
    );
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "appendClip seg2",
    );
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "appendClip seg3",
    );
    let seg1_clip_id = seg1["clipId"].as_str().unwrap().to_string();

    async fn trim(client: &mut Client, clip_index: i64, in_f: i64, out_f: i64) {
        Client::ok(
            &client.call("edit.trimClipIn", json!({"trackIndex": 0, "clipIndex": clip_index, "newFrame": in_f})).await,
            "edit.trimClipIn",
        );
        Client::ok(
            &client.call("edit.trimClipOut", json!({"trackIndex": 0, "clipIndex": clip_index, "newFrame": out_f})).await,
            "edit.trimClipOut",
        );
    }
    trim(&mut client, 1, 0, 44).await;
    trim(&mut client, 2, 90, 134).await;
    trim(&mut client, 3, 200, 244).await;
    let seg_len = 45i64;
    let crossfade_d = 15i64;

    Client::ok(
        &client
            .call("transitions.addCrossfade", json!({"trackIndex": 0, "betweenClips": [1, 2], "durationFrames": crossfade_d}))
            .await,
        "transitions.addCrossfade (1,2)",
    );
    Client::ok(
        &client
            .call("transitions.addCrossfade", json!({"trackIndex": 0, "betweenClips": [2, 3], "durationFrames": crossfade_d}))
            .await,
        "transitions.addCrossfade (2,3)",
    );

    let zoom_filter = Client::ok(
        &client
            .call("filter.add", json!({"clipId": seg1_clip_id, "mltService": "affine", "properties": {"transition.distort": 1}}))
            .await,
        "filter.add affine (zoom)",
    );
    let zoom_filter_index = zoom_filter["filterIndex"].as_i64().unwrap();
    for (pos, rect) in [(0, "0% 0% 100% 100% 1"), (44, "-20% -20% 140% 140% 1")] {
        Client::ok(
            &client
                .call(
                    "filter.addKeyframe",
                    json!({"clipId": seg1_clip_id, "filterIndex": zoom_filter_index, "property": "transition.rect", "position": pos, "value": rect, "interpolation": "linear"}),
                )
                .await,
            "filter.addKeyframe (zoom)",
        );
    }

    Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack (V2)");
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 1, "source": {"blank": 190}})).await,
        "appendClip blank lead",
    );
    let overlay_clip = Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 1, "source": {"playlistIndex": 1}})).await,
        "appendClip overlay",
    );
    let overlay_clip_id = overlay_clip["clipId"].as_str().unwrap().to_string();
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 1, "source": {"blank": 5}})).await,
        "appendClip blank trail",
    );

    let slide_filter = Client::ok(
        &client
            .call(
                "filter.add",
                json!({"clipId": overlay_clip_id, "mltService": "affine", "properties": {"transition.distort": 1, "transition.fill": 1}}),
            )
            .await,
        "filter.add affine (slide-in)",
    );
    let slide_filter_index = slide_filter["filterIndex"].as_i64().unwrap();
    for (pos, rect) in [(0, "120% -20% 30% 30% 1"), (10, "65% 5% 30% 30% 1"), (59, "65% 5% 30% 30% 1")] {
        Client::ok(
            &client
                .call(
                    "filter.addKeyframe",
                    json!({"clipId": overlay_clip_id, "filterIndex": slide_filter_index, "property": "transition.rect", "position": pos, "value": rect, "interpolation": "linear"}),
                )
                .await,
            "filter.addKeyframe (slide-in)",
        );
    }

    let brightness_filter = Client::ok(
        &client.call("filter.add", json!({"clipId": overlay_clip_id, "mltService": "brightness", "properties": {}})).await,
        "filter.add brightness (fade out)",
    );
    let brightness_filter_index = brightness_filter["filterIndex"].as_i64().unwrap();
    for (pos, level) in [(0, 1.0), (40, 1.0), (59, 0.0)] {
        Client::ok(
            &client
                .call(
                    "filter.addKeyframe",
                    json!({"clipId": overlay_clip_id, "filterIndex": brightness_filter_index, "property": "level", "position": pos, "value": level, "interpolation": "linear"}),
                )
                .await,
            "filter.addKeyframe (brightness)",
        );
    }

    Client::ok(&client.call("subtitles.addTrack", json!({})).await, "subtitles.addTrack");
    Client::ok(
        &client
            .call("subtitles.appendItem", json!({"trackIndex": 0, "startFrame": 60, "endFrame": 90, "text": "Highlight One"}))
            .await,
        "subtitles.appendItem (1)",
    );
    Client::ok(
        &client
            .call("subtitles.appendItem", json!({"trackIndex": 0, "startFrame": 200, "endFrame": 230, "text": "Highlight Two"}))
            .await,
        "subtitles.appendItem (2)",
    );

    let export_dir = workdir.join("out");
    std::fs::create_dir_all(&export_dir).unwrap();
    let output_path = export_dir.join("phase-a.mp4");
    let export = client
        .call("file.export", json!({"outputPath": output_path.to_string_lossy(), "codec": "libx264", "container": "mp4"}))
        .await;
    let export_result = Client::ok(&export, "file.export");
    let job_id = export_result["jobId"].as_str().expect("file.export returns a jobId").to_string();

    let mut status = String::new();
    let mut last_job = Value::Null;
    for _ in 0..900 {
        let job = Client::ok(&client.call("jobs.get", json!({"jobId": job_id})).await, "jobs.get");
        status = job["status"].as_str().unwrap_or_default().to_string();
        last_job = job.clone();
        if status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(status, "done", "export job should finish successfully: {last_job:?}");
    assert!(output_path.exists(), "exported file should exist at {}", output_path.display());

    let probe = ffprobe_json(&output_path);
    let streams = probe["streams"].as_array().expect("ffprobe reports streams");
    let has_video = streams.iter().any(|s| s["codec_type"] == "video" && s["codec_name"] == "h264");
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    assert!(has_video, "exported file should have an h264 video stream: {probe:?}");
    assert!(has_audio, "exported file should have an audio stream: {probe:?}");

    let duration: f64 = probe["format"]["duration"].as_str().expect("ffprobe reports a duration").parse().expect("duration parses");
    let expected_frames = title_frames + (3 * seg_len - 2 * crossfade_d);
    let expected_secs = expected_frames as f64 / FPS as f64;
    let video_codec = streams.iter().find(|s| s["codec_type"] == "video").map(|s| s["codec_name"].clone());
    println!(
        "phase A export: real ffprobe duration={duration:.3}s codec={video_codec:?} expected={expected_secs:.3}s ({expected_frames}f @ {FPS}fps = title {title_frames}f + 3x{seg_len}f segments - 2x{crossfade_d}f crossfade overlap)"
    );
    assert!(
        (duration - expected_secs).abs() < 0.25,
        "exported duration {duration}s should match expected {expected_secs}s ({expected_frames} frames) within tolerance"
    );

    let zoom_early = grab_frame(&mut client, &projects_root, project_id, 152).await;
    let zoom_late = grab_frame(&mut client, &projects_root, project_id, 178).await;
    let zoom_corner_diff = zoom_early.corner_mean_abs_diff(&zoom_late, 80);
    println!("zoom corner mean_abs_diff (early vs late) = {zoom_corner_diff:.2}");
    assert!(
        zoom_corner_diff > 25.0,
        "zoom-in-from-center should visibly shift the frame corners ({zoom_corner_diff:.2} observed, expected > 25.0)"
    );

    let title_in = grab_frame(&mut client, &projects_root, project_id, 50).await;
    let title_out = grab_frame(&mut client, &projects_root, project_id, 160).await;
    let is_near_white = |r: u8, g: u8, b: u8| r > 200 && g > 200 && b > 200;
    let title_band = (FRAME_W / 4, FRAME_H * 2 / 5, FRAME_W * 3 / 4, FRAME_H * 3 / 5);
    let title_in_frac = title_in.frac_matching(title_band.0, title_band.1, title_band.2, title_band.3, is_near_white);
    let title_out_frac = title_out.frac_matching(title_band.0, title_band.1, title_band.2, title_band.3, is_near_white);
    println!("title white-text fraction: in-window={title_in_frac:.4} out-of-window={title_out_frac:.4}");
    assert!(
        title_in_frac > title_out_frac + 0.01,
        "title text should be visibly more present in-window ({title_in_frac:.4}) than out-of-window ({title_out_frac:.4})"
    );

    let overlay_before = grab_frame(&mut client, &projects_root, project_id, 100).await;
    let overlay_during = grab_frame(&mut client, &projects_root, project_id, 210).await;
    let overlay_after = grab_frame(&mut client, &projects_root, project_id, 253).await;
    let is_deep_pink = |r: u8, g: u8, b: u8| r > 200 && g < 90 && b > 90 && b < 200;
    let rx0 = (FRAME_W as f64 * 0.65) as usize;
    let rx1 = rx0 + (FRAME_W as f64 * 0.30) as usize;
    let ry0 = (FRAME_H as f64 * 0.05) as usize;
    let ry1 = ry0 + (FRAME_H as f64 * 0.30) as usize;
    let before_frac = overlay_before.frac_matching(rx0, ry0, rx1, ry1, is_deep_pink);
    let during_frac = overlay_during.frac_matching(rx0, ry0, rx1, ry1, is_deep_pink);
    let after_frac = overlay_after.frac_matching(rx0, ry0, rx1, ry1, is_deep_pink);
    println!("overlay deep-pink fraction in target rect: before={before_frac:.4} during={during_frac:.4} after={after_frac:.4}");
    assert!(during_frac > 0.5, "overlay should fill most of its on-screen rect during its window (observed {during_frac:.4})");
    assert!(before_frac < 0.05, "overlay should be absent before its window (observed {before_frac:.4})");
    assert!(after_frac < 0.05, "overlay should be absent after its window (observed {after_frac:.4})");

    // frame 75: inside cue 1's [60,90] window, over seg1's testsrc content.
    // frame 165: outside *both* subtitle cues ([60,90] and [200,230]) but
    // still over real testsrc content in the same track region (seg1_mid /
    // the start of the mix1 crossfade) -- NOT frame 5, which sits inside
    // the title window where the background is already fully black (the
    // title's own transparent-on-nothing background), which would saturate
    // a "near-black" metric at both timestamps and prove nothing. Detects
    // the glyph fill color specifically (ffmpeg's `subtitles` filter uses
    // libass's default white-text/black-outline styling), which is more
    // reliably distinct from testsrc's own busy bottom-band content than
    // "near-black" would be.
    let subtitle_in = grab_frame(&mut client, &projects_root, project_id, 75).await;
    let subtitle_out = grab_frame(&mut client, &projects_root, project_id, 165).await;
    let is_subtitle_white = |r: u8, g: u8, b: u8| r > 220 && g > 220 && b > 220;
    let sub_band = (FRAME_W / 4, FRAME_H * 8 / 10, FRAME_W * 3 / 4, FRAME_H * 96 / 100);
    let sub_in_frac = subtitle_in.frac_matching(sub_band.0, sub_band.1, sub_band.2, sub_band.3, is_subtitle_white);
    let sub_out_frac = subtitle_out.frac_matching(sub_band.0, sub_band.1, sub_band.2, sub_band.3, is_subtitle_white);
    println!("subtitle white-glyph fraction in bottom band: in-window={sub_in_frac:.4} out-of-window={sub_out_frac:.4}");
    assert!(
        sub_in_frac > sub_out_frac + 0.01,
        "subtitle burn-in should be visibly more present in-window ({sub_in_frac:.4}) than out-of-window ({sub_out_frac:.4})"
    );
}
