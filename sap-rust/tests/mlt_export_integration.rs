//! Integration test for `MltBackend`: the closest thing to
//! `11-e2e-scenario-tests.md`'s Phase A workflow runnable without a live
//! Qt/Shotcut GUI process. Drives the *real* server over a real Unix
//! socket (same framing/protocol types + client shape as
//! `server_integration.rs`), with a real `melt` subprocess doing the actual
//! rendering -- the assertions at the bottom shell out to `ffprobe` against
//! a real file on disk, not anything mocked.
//!
//! What's simulated here (documented, not hidden): there is no live Qt/
//! QUndoStack anywhere in this test's path -- `MltBackend` is a pure Rust +
//! `melt`/`ffprobe` implementation, independent of the `shotcut/` fork
//! entirely. What's real: the MLT XML generated from the RPC calls below,
//! the `melt` render it triggers, and the resulting MP4 file's actual
//! duration/codecs as reported by a real `ffprobe` run.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sap_rust::framing;
use sap_rust::mlt_backend::MltBackend;
use sap_rust::protocol::{error_codes, RpcRequest, RpcResponse};
use sap_rust::server::{self, ServerConfig};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const TOKEN: &str = "mlt-test-token";
/// Matches `MltBackend`'s fixed project frame rate (`DEFAULT_FPS` in
/// `mlt_backend.rs`) -- the synthetic source below is generated at this
/// exact rate so `MltBackend`'s duration-from-fps math is exact, not
/// approximate.
const PROJECT_FPS: u32 = 30;

fn unique_tag(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4())
}

fn temp_socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sap-rust-mlt-test-{}.sock", unique_tag(tag)))
}

/// Generates a real, short H.264+AAC test source with `ffmpeg`'s `lavfi`
/// test pattern + tone generators -- exactly the "generate one with ffmpeg
/// lavfi testsrc at test setup" the task asked for, not a checked-in fixture.
fn generate_test_source(dir: &std::path::Path, duration_secs: u32) -> PathBuf {
    let path = dir.join("source.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=640x360:rate={PROJECT_FPS}:duration={duration_secs}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={duration_secs}"),
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
        .expect("failed to spawn ffmpeg to generate the test source");
    assert!(status.success(), "ffmpeg failed to generate the synthetic test source");
    assert!(path.exists(), "ffmpeg reported success but {} doesn't exist", path.display());
    path
}

fn ffprobe_json(path: &std::path::Path) -> Value {
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

/// Spins up a real server, backed by a real `MltBackend`, on a temp Unix
/// socket -- same pattern as `server_integration.rs`'s `start_server`, just
/// with `MltBackend` instead of `MockBackend`.
async fn start_server(tag: &str, projects_root: PathBuf) -> PathBuf {
    let socket_path = temp_socket_path(tag);
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        token: TOKEN.to_string(),
        audio_enabled: false,
    };
    let backend = MltBackend::new(projects_root);
    tokio::spawn(async move {
        let _ = server::serve(config, backend, None).await;
    });
    for _ in 0..100 {
        if socket_path.exists() {
            return socket_path;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not bind {} in time", socket_path.display());
}

/// Same thin real client as `server_integration.rs` (kept duplicated rather
/// than shared, since `tests/` binaries don't share code without a helper
/// module -- not worth restructuring for one small struct).
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
                continue; // unsolicited notification, keep waiting for our response
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

/// (c) any project-scoped call before project.select is rejected, using one
/// of the new doc-11 methods specifically (not just the original surface
/// `server_integration.rs` already checked).
#[tokio::test]
async fn playlist_append_before_project_select_is_rejected() {
    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-noselect"));
    let path = start_server("noselect", projects_root).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;

    let resp = client.call("playlist.append", json!({"source": {"path": "/tmp/does-not-matter.mp4"}})).await;
    let err = resp.error.expect("playlist.append before project.select must be rejected");
    assert_eq!(err.code, error_codes::NO_PROJECT_BOUND);
}

#[tokio::test]
async fn file_import_rejects_paths_outside_bound_project_root() {
    let projects_root = std::env::temp_dir().join(unique_tag("file-import-sandbox"));
    let other_project_root = projects_root.join("other-project");
    std::fs::create_dir_all(&other_project_root).expect("create sibling project root");
    let sibling_file = other_project_root.join("asset.mp4");
    std::fs::write(&sibling_file, b"not imported").expect("create sibling project asset");

    let socket = start_server("file-import-sandbox", projects_root.clone()).await;
    let mut client = Client::connect(&socket).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "bound-project"})).await;

    for outside_path in ["/etc/passwd".to_string(), sibling_file.to_string_lossy().into_owned()] {
        let response = client.call("file.import", json!({"path": outside_path})).await;
        let error = response.error.expect("outside file.import path must be rejected");
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
        assert!(
            error.message.contains("outside project root"),
            "unexpected sandbox error: {}",
            error.message
        );
    }

    let _ = std::fs::remove_dir_all(projects_root);
}

/// (b) edit.appendClip round trip via the new playlist.append -> playlistIndex
/// path -- the actual workflow doc 11 Phase A uses (playlist bin, not a bare
/// path, feeding the timeline).
#[tokio::test]
async fn playlist_append_and_append_clip_round_trip() {
    let workdir = std::env::temp_dir().join(unique_tag("mlt-workdir-playlist"));
    std::fs::create_dir_all(&workdir).unwrap();
    let source = generate_test_source(&workdir, 2);

    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-playlist"));
    let path = start_server("playlist", projects_root).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "playlist-proj"})).await;

    let appended = client.call("playlist.append", json!({"source": {"path": source.to_string_lossy()}})).await;
    let entry = Client::ok(&appended, "playlist.append");
    assert_eq!(entry["index"], 0);
    // 2s @ 30fps == 60 frames, from real ffprobe of the generated source.
    assert_eq!(entry["durationFrames"], 60);

    let track = Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack");
    assert_eq!(track["index"], 0);

    let clip = Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "edit.appendClip",
    );
    assert_eq!(clip["outFrame"], 59);
    assert!(clip["clipId"].as_str().unwrap().starts_with("clip-"));

    let clips = Client::ok(&client.call("edit.listClips", json!({"trackIndex": 0})).await, "edit.listClips");
    let clips = clips.as_array().unwrap();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0]["clipId"], clip["clipId"]);
}

/// (d) multi-client notification fan-out for one of the new mutating
/// methods (playlist.append -> playlist.changed), same pattern as
/// `server_integration.rs`'s `edit_add_track_fans_out_to_other_client_on_same_project`.
#[tokio::test]
async fn playlist_append_fans_out_to_other_client_on_same_project() {
    let workdir = std::env::temp_dir().join(unique_tag("mlt-workdir-fanout"));
    std::fs::create_dir_all(&workdir).unwrap();
    let source = generate_test_source(&workdir, 1);

    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-fanout"));
    let path = start_server("fanout", projects_root).await;

    let mut client_a = Client::connect(&path).await;
    client_a.call("sap.hello", json!({"token": TOKEN})).await;
    client_a.call("project.select", json!({"projectId": "shared-mlt-proj"})).await;

    let mut client_b = Client::connect(&path).await;
    client_b.call("sap.hello", json!({"token": TOKEN})).await;
    client_b.call("project.select", json!({"projectId": "shared-mlt-proj"})).await;

    let appended = client_a.call("playlist.append", json!({"source": {"path": source.to_string_lossy()}})).await;
    assert!(appended.error.is_none(), "client A's playlist.append should succeed: {:?}", appended.error);

    let notification = loop {
        let value = tokio::time::timeout(Duration::from_secs(2), framing::read_message(&mut client_b.reader))
            .await
            .expect("client B should receive a fan-out notification before timing out")
            .expect("read notification frame");
        if value.get("id").is_none() {
            break value;
        }
    };
    assert_eq!(notification["method"], "playlist.changed");
}

/// The full pipeline: (a)+(b) setup, then generator.createTitle, file.export
/// (a real background `melt` job), jobs.get polling, and real `ffprobe`
/// assertions against the actual exported file -- the closest thing to
/// 11-e2e-scenario-tests.md's Phase A runnable without a live Qt process.
#[tokio::test]
async fn full_export_pipeline_produces_a_real_playable_file() {
    let workdir = std::env::temp_dir().join(unique_tag("mlt-workdir-export"));
    std::fs::create_dir_all(&workdir).unwrap();
    let clip_duration_secs = 2;
    let source = generate_test_source(&workdir, clip_duration_secs);

    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-export"));
    let path = start_server("export", projects_root.clone()).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    let select = client.call("project.select", json!({"projectId": "export-proj"})).await;
    assert!(select.error.is_none(), "project.select should succeed: {:?}", select.error);

    // playlist.append + edit.appendClip the synthetic source, per the task's
    // required scenario.
    let appended = client.call("playlist.append", json!({"source": {"path": source.to_string_lossy()}})).await;
    Client::ok(&appended, "playlist.append");

    Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack (V1)");

    // Title first, then the source clip, both on the *same* track, in
    // sequence -- not on a separate overlay track. A second video track
    // would be composited *simultaneously* with track 0 by MLT's tractor
    // (multi-track timelines overlay, they don't concatenate), so total
    // duration would be max(title, clip) instead of a real end-to-end
    // sequence; putting both on one track is what actually exercises (and
    // lets ffprobe verify) concatenated real duration end to end.
    let title_entry = Client::ok(
        &client
            .call("generator.createTitle", json!({"mode": "simple", "text": "Highlights"}))
            .await,
        "generator.createTitle",
    );
    assert_eq!(title_entry["index"], 1, "title should land in the playlist bin after the source clip");
    let title_duration_frames = title_entry["durationFrames"].as_i64().expect("durationFrames is an int");
    assert!(title_duration_frames > 0);

    Client::ok(
        &client
            .call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 1}}))
            .await,
        "edit.appendClip (title)",
    );
    let clip = Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "edit.appendClip (source clip, after the title)",
    );

    // filter.add + filter.addKeyframe on the source clip, so the export
    // also proves the attached-filter XML path (real doc-11 zoom-in
    // example), not just bare clips.
    let clip_id = clip["clipId"].as_str().expect("clip has a clipId").to_string();
    let filter = Client::ok(
        &client.call("filter.add", json!({"clipId": clip_id, "mltService": "affine", "properties": {}})).await,
        "filter.add",
    );
    let filter_index = filter["filterIndex"].as_i64().unwrap();
    Client::ok(
        &client
            .call(
                "filter.addKeyframe",
                json!({
                    "clipId": clip_id,
                    "filterIndex": filter_index,
                    "property": "transition.geometry",
                    "position": 0,
                    "value": "50%,50%:10%x10%",
                    "interpolation": "linear",
                }),
            )
            .await,
        "filter.addKeyframe",
    );

    let export_dir = workdir.join("out");
    std::fs::create_dir_all(&export_dir).unwrap();
    let output_path = export_dir.join("highlight-reel.mp4");
    let export = client
        .call(
            "file.export",
            json!({"outputPath": output_path.to_string_lossy(), "codec": "libx264", "container": "mp4"}),
        )
        .await;
    let export_result = Client::ok(&export, "file.export");
    let job_id = export_result["jobId"].as_str().expect("file.export returns a jobId").to_string();

    // jobs.get polling until the real melt subprocess finishes, per doc 01's
    // async-job convention.
    let mut status = String::new();
    let mut last_status_json = Value::Null;
    for _ in 0..600 {
        let job = Client::ok(&client.call("jobs.get", json!({"jobId": job_id})).await, "jobs.get");
        status = job["status"].as_str().unwrap_or_default().to_string();
        last_status_json = job.clone();
        if status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(status, "done", "export job should finish successfully: {last_status_json:?}");

    // The real proof: an actual file on disk, inspected with a real
    // ffprobe run (not anything the server/backend reported about itself).
    assert!(output_path.exists(), "exported file should exist at {}", output_path.display());
    let probe = ffprobe_json(&output_path);

    let streams = probe["streams"].as_array().expect("ffprobe reports streams");
    let has_video = streams.iter().any(|s| s["codec_type"] == "video" && s["codec_name"] == "h264");
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    assert!(has_video, "exported file should have an h264 video stream: {probe:?}");
    assert!(has_audio, "exported file should have an audio stream: {probe:?}");

    let duration: f64 = probe["format"]["duration"]
        .as_str()
        .expect("ffprobe reports a duration")
        .parse()
        .expect("duration parses as f64");
    // Expected total duration: title (title_duration_frames) + source clip
    // (clip_duration_secs * PROJECT_FPS frames), at PROJECT_FPS.
    let expected_frames = title_duration_frames + (clip_duration_secs as i64 * PROJECT_FPS as i64);
    let expected_secs = expected_frames as f64 / PROJECT_FPS as f64;
    assert!(
        (duration - expected_secs).abs() < 0.5,
        "exported duration {duration}s should be close to expected {expected_secs}s (title {title_duration_frames}f + clip {clip_duration_secs}s @ {PROJECT_FPS}fps)"
    );

    // playback.getFrame: real melt single-frame grab, mid-timeline (inside
    // the source clip's region, after the title).
    let mid_frame = title_duration_frames + 10;
    let frame = Client::ok(
        &client.call("playback.getFrame", json!({"frame": mid_frame, "format": "jpeg"})).await,
        "playback.getFrame",
    );
    let data_b64 = frame["data"].as_str().expect("playback.getFrame returns base64 data");
    assert!(!data_b64.is_empty(), "grabbed frame payload should not be empty");
}

/// Regression test for a real bug a live `claude -p` MCP agent run
/// surfaced against this exact codebase: the agent naturally supplied
/// `codec: "h264"` (matching `memory/head/gen/rust-fork/11-e2e-scenario-tests.md`'s
/// own `file.export` example), but `melt`'s `avformat` consumer only
/// recognizes the real libavcodec encoder name `libx264` -- with the raw
/// `codec` param passed straight through, `melt` logged
/// "h264 unrecognised - ignoring", silently dropped the entire video
/// stream, and still exited 0, so `jobs.get` reported "done" on an
/// audio-only file. Every other test in this file sidesteps the bug by
/// hardcoding `codec: "libx264"` directly, which is why it went
/// undetected until a real end-to-end agent transcript hit it. This test
/// uses the exact spec-level value an agent would supply.
#[tokio::test]
async fn file_export_normalizes_spec_level_codec_name_to_a_real_encoder() {
    let workdir = std::env::temp_dir().join(unique_tag("mlt-workdir-codec-alias"));
    std::fs::create_dir_all(&workdir).unwrap();
    let source = generate_test_source(&workdir, 2);

    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-codec-alias"));
    let path = start_server("codec-alias", projects_root.clone()).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    let select = client.call("project.select", json!({"projectId": "codec-alias-proj"})).await;
    assert!(select.error.is_none(), "project.select should succeed: {:?}", select.error);

    Client::ok(
        &client.call("playlist.append", json!({"source": {"path": source.to_string_lossy()}})).await,
        "playlist.append",
    );
    Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack");
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "edit.appendClip",
    );

    let export_dir = workdir.join("out");
    std::fs::create_dir_all(&export_dir).unwrap();
    let output_path = export_dir.join("h264-alias.mp4");
    let export = client
        .call(
            "file.export",
            // Deliberately "h264", not "libx264" -- the exact value the
            // e2e scenario doc and a real agent both used.
            json!({"outputPath": output_path.to_string_lossy(), "codec": "h264", "container": "mp4"}),
        )
        .await;
    let export_result = Client::ok(&export, "file.export");
    let job_id = export_result["jobId"].as_str().expect("file.export returns a jobId").to_string();

    let mut status = String::new();
    let mut last_status_json = Value::Null;
    for _ in 0..600 {
        let job = Client::ok(&client.call("jobs.get", json!({"jobId": job_id})).await, "jobs.get");
        status = job["status"].as_str().unwrap_or_default().to_string();
        last_status_json = job.clone();
        if status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(status, "done", "export job should finish successfully: {last_status_json:?}");

    assert!(output_path.exists(), "exported file should exist at {}", output_path.display());
    let probe = ffprobe_json(&output_path);
    let streams = probe["streams"].as_array().expect("ffprobe reports streams");
    let has_video = streams.iter().any(|s| s["codec_type"] == "video");
    assert!(
        has_video,
        "exported file must have a real video stream even when codec was requested as \"h264\" \
         (the raw spec-level name, not the encoder name \"libx264\"): {probe:?}"
    );
    let has_h264 = streams.iter().any(|s| s["codec_type"] == "video" && s["codec_name"] == "h264");
    assert!(has_h264, "video stream should actually be encoded as h264: {probe:?}");
}

/// Full RPC round trip for the newly-added `playlist.*` methods (task 1):
/// `insert`/`remove`/`move`/`get`/`addToTimeline`, against a real
/// `MltBackend` over a real socket -- mirrors
/// `playlist_append_and_append_clip_round_trip`'s style/level, extended to
/// the rest of the namespace. `playlist.get`'s real `ffprobe`-backed probe
/// data is asserted explicitly (reusing `file.probe`'s own helper, per the
/// task), not just the bare entry shape `playlist.append`/`list` return.
#[tokio::test]
async fn playlist_insert_remove_move_get_and_add_to_timeline_round_trip() {
    let workdir = std::env::temp_dir().join(unique_tag("mlt-workdir-playlist-ext"));
    std::fs::create_dir_all(&workdir).unwrap();
    let source_a = generate_test_source(&workdir, 1); // 30 frames

    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-playlist-ext"));
    let path = start_server("playlist-ext", projects_root).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "playlist-ext-proj"})).await;

    // Seed with two blank spacers (index 0, 1), then insert a real source
    // clip between them at index 1.
    Client::ok(&client.call("playlist.append", json!({"source": {"blank": 10}, "name": "a"})).await, "append a");
    Client::ok(&client.call("playlist.append", json!({"source": {"blank": 40}, "name": "c"})).await, "append c");
    let inserted = Client::ok(
        &client
            .call("playlist.insert", json!({"index": 1, "source": {"path": source_a.to_string_lossy()}, "name": "b"}))
            .await,
        "playlist.insert",
    );
    assert_eq!(inserted["index"], 1);
    assert_eq!(inserted["name"], "b");
    assert_eq!(inserted["durationFrames"], 30);

    let listed = Client::ok(&client.call("playlist.list", json!({})).await, "playlist.list");
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);

    // playlist.get: full metadata including real ffprobe-derived probe
    // data for the real source clip at index 1.
    let got = Client::ok(&client.call("playlist.get", json!({"index": 1})).await, "playlist.get");
    assert_eq!(got["name"], "b");
    let probe = got.get("probe").expect("playlist.get should include probe data for a real file source");
    assert_eq!(probe["durationFrames"], 30);
    assert!(probe["codec"].as_str().is_some_and(|c| !c.is_empty()), "probe.codec should be a real codec name: {probe:?}");

    // A blank spacer has nothing real to probe -- honestly None/absent,
    // not fabricated.
    let blank_detail = Client::ok(&client.call("playlist.get", json!({"index": 0})).await, "playlist.get (blank)");
    assert!(blank_detail.get("probe").is_none(), "blank spacer entries should have no probe data: {blank_detail:?}");

    let missing = client.call("playlist.get", json!({"index": 99})).await;
    assert!(missing.error.is_some(), "playlist.get on an out-of-range index must fail");

    // playlist.move: move "c" (index 2) to the front.
    Client::ok(&client.call("playlist.move", json!({"fromIndex": 2, "toIndex": 0})).await, "playlist.move");
    let listed = Client::ok(&client.call("playlist.list", json!({})).await, "playlist.list (after move)");
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["c", "a", "b"]);

    // playlist.addToTimeline: convenience wrapper equivalent to
    // edit.appendClip({source: {playlistIndex}}) -- "b" is now at index 2.
    Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack");
    let clip = Client::ok(
        &client.call("playlist.addToTimeline", json!({"index": 2, "trackIndex": 0, "position": 0})).await,
        "playlist.addToTimeline",
    );
    assert_eq!(clip["outFrame"], 29, "addToTimeline'd clip should resolve to \"b\"'s real 30-frame source");
    let clips = Client::ok(&client.call("edit.listClips", json!({"trackIndex": 0})).await, "edit.listClips");
    assert_eq!(clips.as_array().unwrap().len(), 1);

    // playlist.remove: remove "a" (now index 1).
    Client::ok(&client.call("playlist.remove", json!({"index": 1})).await, "playlist.remove");
    let listed = Client::ok(&client.call("playlist.list", json!({})).await, "playlist.list (after remove)");
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["c", "b"]);

    let bad_remove = client.call("playlist.remove", json!({"index": 99})).await;
    assert!(bad_remove.error.is_some());
}

/// Generates a real, CPU-heavy H.264 source (a fractal pattern that's hard
/// for `libx264` to compress) that reliably takes several real seconds for
/// `melt` to re-encode -- long enough to call `jobs.stop` mid-flight and
/// prove it kills a genuinely in-flight process, not just win a race
/// against an export that was always going to finish before the stop call
/// landed. Empirically measured during development: this exact
/// 1920x1080/30s/mandelbrot config takes ~5s wall-clock for `melt`'s
/// default libx264 re-encode on the dev machine (20 cores) -- ample margin
/// over the sub-millisecond local Unix-socket round trip `jobs.stop` needs.
fn generate_slow_test_source(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("slow-source.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("mandelbrot=size=1920x1080:rate={PROJECT_FPS}"),
            "-t",
            "30",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-loglevel",
            "error",
        ])
        .arg(&path)
        .status()
        .expect("failed to spawn ffmpeg to generate the slow test source");
    assert!(status.success(), "ffmpeg failed to generate the slow synthetic test source");
    assert!(path.exists());
    path
}

/// Returns the pid of the first process whose command line contains
/// `needle`, if any -- used below to (a) confirm a real `melt` process is
/// actually running before `jobs.stop` is called, and (b) confirm that
/// exact pid is fully gone (not lingering as a zombie/orphan) afterward.
fn pgrep_pid(needle: &str) -> Option<u32> {
    let output = Command::new("pgrep").args(["-f"]).arg(needle).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().next()?.trim().parse().ok()
}

/// Proof for testing-plan.md Phase 3's `jobs.*` row + task 2c: `jobs.stop`
/// against a *real* in-flight `file.export`, not just the existing
/// `jobs_stop_marks_unknown_as_not_found` not-found-job unit test. Confirms
/// (a) the job's status settles on `"stopped"`, never `"done"`, and (b) the
/// real `melt` subprocess is actually killed -- its pid is fully reaped
/// (no `/proc/<pid>` entry at all, so not even a transient zombie), not
/// merely orphaned and left running in the background.
#[tokio::test]
async fn jobs_stop_kills_a_real_inflight_slow_export() {
    let workdir = std::env::temp_dir().join(unique_tag("mlt-workdir-jobstop"));
    std::fs::create_dir_all(&workdir).unwrap();
    let source = generate_slow_test_source(&workdir);

    let projects_root = std::env::temp_dir().join(unique_tag("mlt-projects-jobstop"));
    let path = start_server("jobstop", projects_root).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "jobstop-proj"})).await;

    Client::ok(
        &client.call("playlist.append", json!({"source": {"path": source.to_string_lossy()}})).await,
        "playlist.append",
    );
    Client::ok(&client.call("edit.addTrack", json!({"kind": "video"})).await, "edit.addTrack");
    Client::ok(
        &client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"playlistIndex": 0}})).await,
        "edit.appendClip",
    );

    let export_dir = workdir.join("out");
    std::fs::create_dir_all(&export_dir).unwrap();
    let output_path = export_dir.join("jobstop-output.mp4");
    let export = client
        .call("file.export", json!({"outputPath": output_path.to_string_lossy(), "codec": "libx264", "container": "mp4"}))
        .await;
    let export_result = Client::ok(&export, "file.export");
    let job_id = export_result["jobId"].as_str().expect("file.export returns a jobId").to_string();

    // Confirm a real melt process is actually running against this exact
    // (UUID-unique) output path before stopping it -- otherwise stopping
    // "too fast" wouldn't prove anything about killing a real in-flight job.
    let output_str = output_path.to_string_lossy().into_owned();
    let mut running_pid = None;
    for _ in 0..40 {
        if let Some(pid) = pgrep_pid(&output_str) {
            running_pid = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let running_pid = running_pid.unwrap_or_else(|| panic!("expected a real melt process running against {output_str} before stopping it"));

    let stop = client.call("jobs.stop", json!({"jobId": job_id})).await;
    Client::ok(&stop, "jobs.stop");

    // Poll jobs.get -- must settle on "stopped", never "done" or "error".
    let mut status = String::new();
    let mut last_job = Value::Null;
    for _ in 0..100 {
        let job = Client::ok(&client.call("jobs.get", json!({"jobId": job_id})).await, "jobs.get");
        status = job["status"].as_str().unwrap_or_default().to_string();
        last_job = job.clone();
        if status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, "stopped", "jobs.stop'd export should settle on status=stopped, never done: {last_job:?}");

    // Real process-kill proof: the exact pid we observed running must be
    // fully reaped -- no /proc entry at all (not running, not a zombie).
    let mut proc_gone = false;
    for _ in 0..40 {
        if !Path::new(&format!("/proc/{running_pid}")).exists() {
            proc_gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(proc_gone, "melt pid {running_pid} should be fully reaped after jobs.stop (no zombie/orphan)");
    assert!(pgrep_pid(&output_str).is_none(), "no melt process should remain running against {output_str} after jobs.stop");
}
