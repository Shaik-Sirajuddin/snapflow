//! Integration tests: real client(s) over a real Unix socket, driving the
//! actual `server::serve` entrypoint end to end (no mocking of the wire
//! layer) against a `MockBackend`.

use std::path::PathBuf;
use std::time::Duration;

use sap_rust::backend::MockBackend;
use sap_rust::framing;
use sap_rust::protocol::{error_codes, RpcNotification, RpcRequest, RpcResponse};
use sap_rust::server::{self, ServerConfig};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const TOKEN: &str = "test-token-123";

fn temp_socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sap-rust-test-{tag}-{}.sock", uuid::Uuid::new_v4()))
}

/// Spins up a real server on a temp Unix socket path in a background task
/// and waits for the socket file to appear before handing control back.
async fn start_server(tag: &str, token: &str) -> PathBuf {
    let socket_path = temp_socket_path(tag);
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        token: token.to_string(),
        audio_enabled: false,
    };
    let backend = MockBackend::new();
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

/// A thin real client over the same framing/protocol types the server uses.
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

    /// Sends one request and waits for its matching response, transparently
    /// skipping over any notifications that arrive first (a notification
    /// fanned out from a concurrent client can interleave with a call's own
    /// response on the wire).
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
                // Unsolicited notification, not this call's response — keep waiting.
                continue;
            }
            let resp: RpcResponse = serde_json::from_value(value).expect("parse response");
            assert_eq!(resp.id, json!(id), "response id must match the request id");
            return resp;
        }
    }

    /// Waits for the next unsolicited notification, ignoring any (further)
    /// call responses in between.
    async fn recv_notification(&mut self) -> RpcNotification {
        loop {
            let value = framing::read_message(&mut self.reader).await.expect("read notification");
            if value.get("id").is_none() {
                return serde_json::from_value(value).expect("parse notification");
            }
        }
    }

    async fn recv_notification_timeout(&mut self, dur: Duration) -> Option<RpcNotification> {
        tokio::time::timeout(dur, self.recv_notification()).await.ok()
    }
}

#[tokio::test]
async fn hello_handshake_and_project_select_round_trip() {
    let path = start_server("hello-select", TOKEN).await;
    let mut client = Client::connect(&path).await;

    let hello = client.call("sap.hello", json!({"token": TOKEN})).await;
    assert!(hello.error.is_none(), "hello should succeed: {:?}", hello.error);

    let select = client.call("project.select", json!({"projectId": "proj-a"})).await;
    assert!(select.error.is_none(), "project.select should succeed: {:?}", select.error);
    let state = select.result.expect("project.select returns a result");
    assert_eq!(state["projectId"], "proj-a");
    assert_eq!(state["dirty"], false);
}

#[tokio::test]
async fn hello_with_bad_token_is_rejected() {
    let path = start_server("bad-token", TOKEN).await;
    let mut client = Client::connect(&path).await;

    let hello = client.call("sap.hello", json!({"token": "wrong"})).await;
    let err = hello.error.expect("bad token must be rejected");
    assert_eq!(err.code, error_codes::BAD_TOKEN);

    // Still unauthenticated afterwards — a project-scoped call must bounce.
    let select = client.call("project.select", json!({"projectId": "proj-a"})).await;
    let err = select.error.expect("unauthenticated call must be rejected");
    assert_eq!(err.code, error_codes::UNAUTHENTICATED);
}

#[tokio::test]
async fn add_track_and_list_tracks_round_trip() {
    let path = start_server("add-list-tracks", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "proj-b"})).await;

    let added = client.call("edit.addTrack", json!({"kind": "video"})).await;
    assert!(added.error.is_none(), "edit.addTrack should succeed: {:?}", added.error);
    let track = added.result.expect("edit.addTrack returns the new track");
    assert_eq!(track["index"], 0);
    assert_eq!(track["kind"], "video");

    let listed = client.call("edit.listTracks", json!({})).await;
    assert!(listed.error.is_none());
    let tracks = listed.result.expect("edit.listTracks returns a list");
    let tracks = tracks.as_array().expect("tracks is a JSON array");
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0]["kind"], "video");
}

#[tokio::test]
async fn track_reorder_properties_and_clip_remove_move_dispatch() {
    let path = start_server("track-clip-ops", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "proj-ops"})).await;

    client.call("edit.addTrack", json!({"kind": "video"})).await;
    client.call("edit.addTrack", json!({"kind": "video"})).await;

    let props = client
        .call(
            "edit.setTrackProperties",
            json!({"trackIndex": 1, "muted": true, "blendMode": "14"}),
        )
        .await;
    assert!(props.error.is_none(), "edit.setTrackProperties should succeed: {:?}", props.error);
    let track = props.result.expect("edit.setTrackProperties returns the updated track");
    assert_eq!(track["muted"], true);
    assert_eq!(track["blendMode"], "14");

    let height = client.call("edit.setTrackHeight", json!({"height": 90})).await;
    assert!(height.error.is_none());

    let reordered = client.call("edit.reorderTrack", json!({"fromIndex": 0, "toIndex": 1})).await;
    assert!(reordered.error.is_none(), "edit.reorderTrack should succeed: {:?}", reordered.error);
    let tracks = reordered.result.expect("edit.reorderTrack returns the new track list");
    let tracks = tracks.as_array().expect("tracks is a JSON array");
    // The muted/blendMode track (originally index 1) is now at index 0.
    assert_eq!(tracks[0]["muted"], true);
    assert_eq!(tracks[0]["blendMode"], "14");

    let clip_a = client
        .call("edit.appendClip", json!({"trackIndex": 0, "source": {"path": "/tmp/a.mp4"}}))
        .await
        .result
        .expect("appendClip a");
    client
        .call("edit.appendClip", json!({"trackIndex": 0, "source": {"path": "/tmp/b.mp4"}}))
        .await;

    let moved = client
        .call(
            "edit.moveClip",
            json!({"fromTrackIndex": 0, "fromClipIndex": 0, "toTrackIndex": 1, "toClipIndex": 0}),
        )
        .await;
    assert!(moved.error.is_none(), "edit.moveClip should succeed: {:?}", moved.error);
    let moved_clip = moved.result.expect("edit.moveClip returns the moved clip");
    assert_eq!(moved_clip["clipId"], clip_a["clipId"]);

    let track1_clips = client.call("edit.listClips", json!({"trackIndex": 1})).await.result.unwrap();
    assert_eq!(track1_clips.as_array().unwrap().len(), 1);

    let removed = client.call("edit.removeClip", json!({"trackIndex": 0, "clipIndex": 0})).await;
    assert!(removed.error.is_none(), "edit.removeClip should succeed: {:?}", removed.error);
    let track0_clips = client.call("edit.listClips", json!({"trackIndex": 0})).await.result.unwrap();
    assert_eq!(track0_clips.as_array().unwrap().len(), 0);

    let bad = client.call("edit.reorderTrack", json!({"fromIndex": 9, "toIndex": 0})).await;
    assert!(bad.error.is_some(), "out-of-range reorder must be rejected");
}

#[tokio::test]
async fn project_scoped_call_before_select_is_rejected() {
    let path = start_server("no-project-bound", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;

    let listed = client.call("edit.listTracks", json!({})).await;
    let err = listed.error.expect("edit.listTracks without project.select must be rejected");
    assert_eq!(err.code, error_codes::NO_PROJECT_BOUND);

    // project.select itself still works afterwards — the rejection is
    // per-call, not a permanent connection failure.
    let select = client.call("project.select", json!({"projectId": "proj-c"})).await;
    assert!(select.error.is_none());
}

#[tokio::test]
async fn reselecting_the_same_project_stays_a_noop_success() {
    let path = start_server("reselect-same-project", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;

    let first = client.call("project.select", json!({"projectId": "proj-same"})).await;
    assert!(first.error.is_none(), "first select should succeed: {:?}", first.error);

    let second = client.call("project.select", json!({"projectId": "proj-same"})).await;
    assert!(second.error.is_none(), "reselecting the same project must stay idempotent: {:?}", second.error);
}

#[tokio::test]
async fn switching_project_without_exit_is_rejected() {
    let path = start_server("switch-without-exit", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;

    let first = client.call("project.select", json!({"projectId": "proj-x"})).await;
    assert!(first.error.is_none(), "first select should succeed: {:?}", first.error);

    let switch = client.call("project.select", json!({"projectId": "proj-y"})).await;
    let err = switch.error.expect("switching project without project.exit must be rejected");
    assert_eq!(err.code, error_codes::ALREADY_BOUND);
    assert!(err.message.contains("proj-x"), "error should name the currently-bound project: {}", err.message);

    // The session must remain usable and still bound to the ORIGINAL
    // project — the rejected attempt must not have partially rebound it.
    let listed = client.call("edit.listTracks", json!({})).await;
    assert!(listed.error.is_none(), "session should still be bound to proj-x after the rejected switch");
}

#[tokio::test]
async fn project_exit_then_select_a_different_project_succeeds() {
    let path = start_server("exit-then-switch", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;

    let first = client.call("project.select", json!({"projectId": "proj-x"})).await;
    assert!(first.error.is_none());

    let exit = client.call("project.exit", json!({})).await;
    assert!(exit.error.is_none(), "project.exit should succeed: {:?}", exit.error);

    let switch = client.call("project.select", json!({"projectId": "proj-y"})).await;
    assert!(switch.error.is_none(), "select after exit must succeed: {:?}", switch.error);

    let listed = client.call("edit.listTracks", json!({})).await;
    assert!(listed.error.is_none(), "session should now be usable against proj-y");
}

#[tokio::test]
async fn file_probe_dispatches_and_mock_reports_unsupported() {
    let path = start_server("file-probe-mock", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;

    let response = client.call("file.probe", json!({"path": "/tmp/source.mp4"})).await;
    let error = response.error.expect("MockBackend must report unsupported file.probe");
    assert_eq!(error.code, error_codes::INTERNAL_ERROR);
    assert!(error.message.contains("file.probe"));
}

#[tokio::test]
async fn file_import_dispatches_to_mock_playlist() {
    let path = start_server("file-import-mock", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "import-project"})).await;

    let response = client.call("file.import", json!({"path": "inside.mp4"})).await;
    assert!(response.error.is_none(), "file.import should succeed: {:?}", response.error);
    let entry = response.result.expect("file.import returns a playlist entry");
    assert_eq!(entry["index"], 0);
    assert_eq!(entry["source"]["path"], "inside.mp4");
}

#[tokio::test]
async fn filter_set_property_dispatches_and_last_write_succeeds() {
    let path = start_server("filter-set-property", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client.call("project.select", json!({"projectId": "filter-project"})).await;
    client.call("edit.addTrack", json!({"kind": "video"})).await;
    let clip = client.call("edit.appendClip", json!({"trackIndex": 0, "source": {"path": "/tmp/source.mp4"}})).await;
    let clip_id = clip.result.unwrap()["clipId"].as_str().unwrap().to_string();
    let filter = client
        .call("filter.add", json!({"clipId": clip_id, "mltService": "brightness", "properties": {}}))
        .await;
    let filter_index = filter.result.unwrap()["filterIndex"].clone();

    for value in [0.25, 0.75] {
        let response = client
            .call(
                "filter.setProperty",
                json!({"clipId": clip_id, "filterIndex": filter_index, "property": "level", "value": value}),
            )
            .await;
        assert!(response.error.is_none(), "filter.setProperty should succeed: {:?}", response.error);
    }
}

#[tokio::test]
async fn audio_set_gain_is_not_callable_when_audio_is_disabled() {
    let path = start_server("audio-disabled", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client
        .call("project.select", json!({"projectId": "audio-project"}))
        .await;
    client.call("edit.addTrack", json!({"kind": "video"})).await;
    let clip = client
        .call(
            "edit.appendClip",
            json!({"trackIndex": 0, "source": {"path": "/tmp/source.mp4"}}),
        )
        .await;
    let clip_id = clip.result.unwrap()["clipId"].as_str().unwrap().to_string();

    for (method, params) in [
        ("audio.setGain", json!({"clipId": clip_id, "db": -9})),
        ("audio.setPan", json!({"clipId": clip_id, "pan": 0.25})),
        ("audio.setBalance", json!({"clipId": clip_id, "balance": 0.75})),
        ("audio.setNormalize", json!({"clipId": clip_id, "mode": "1pass"})),
        ("audio.setFadeInOut", json!({"clipId": clip_id, "fadeInFrames": 10})),
        ("audio.setAutoFade", json!({"clipId": clip_id, "enabled": true})),
    ] {
        let response = client.call(method, params).await;
        let error = response
            .error
            .unwrap_or_else(|| panic!("disabled {method} must be rejected"));
        assert_eq!(
            error.code,
            error_codes::METHOD_NOT_FOUND,
            "{method} should be method-not-found when audio is disabled"
        );
        assert!(
            error.message.contains(method),
            "error for {method} should mention the method, got {}",
            error.message
        );
    }
}

#[tokio::test]
async fn edit_add_track_fans_out_to_other_client_on_same_project() {
    let path = start_server("fanout", TOKEN).await;

    let mut client_a = Client::connect(&path).await;
    client_a.call("sap.hello", json!({"token": TOKEN})).await;
    client_a.call("project.select", json!({"projectId": "shared-project"})).await;

    let mut client_b = Client::connect(&path).await;
    client_b.call("sap.hello", json!({"token": TOKEN})).await;
    client_b.call("project.select", json!({"projectId": "shared-project"})).await;

    let added = client_a.call("edit.addTrack", json!({"kind": "audio"})).await;
    assert!(added.error.is_none(), "client A's edit.addTrack should succeed: {:?}", added.error);

    // Client B never called edit.addTrack itself — it must still see the
    // change via an unsolicited edit.changed notification.
    let notification = client_b
        .recv_notification_timeout(Duration::from_secs(2))
        .await
        .expect("client B should receive a fan-out notification");
    assert_eq!(notification.method, "edit.changed");
    assert_eq!(notification.params["reason"], "addTrack");
    assert_eq!(notification.params["trackIndex"], 0);
}

#[tokio::test]
async fn edit_split_clip_and_filter_lifecycle_dispatch() {
    let path = start_server("split-filter-lifecycle", TOKEN).await;
    let mut client = Client::connect(&path).await;
    client.call("sap.hello", json!({"token": TOKEN})).await;
    client
        .call("project.select", json!({"projectId": "split-filter-project"}))
        .await;
    client.call("edit.addTrack", json!({"kind": "video"})).await;
    let clip = client
        .call(
            "edit.appendClip",
            json!({"trackIndex": 0, "source": {"path": "/tmp/source.mp4"}}),
        )
        .await;
    let clip_id = clip.result.unwrap()["clipId"].as_str().unwrap().to_string();
    client
        .call(
            "edit.trimClipIn",
            json!({"trackIndex": 0, "clipIndex": 0, "newFrame": 0}),
        )
        .await;
    client
        .call(
            "edit.trimClipOut",
            json!({"trackIndex": 0, "clipIndex": 0, "newFrame": 99}),
        )
        .await;

    let split = client
        .call(
            "edit.splitClip",
            json!({"trackIndex": 0, "clipIndex": 0, "position": 40}),
        )
        .await;
    assert!(split.error.is_none(), "edit.splitClip should succeed: {:?}", split.error);
    let split_result = split.result.unwrap();
    assert_eq!(split_result["leftClipId"], clip_id);
    assert_eq!(split_result["leftIndex"], 0);
    assert_eq!(split_result["rightIndex"], 1);
    assert!(split_result["rightClipId"].as_str().unwrap().len() > 0);

    let listed = client
        .call("edit.listClips", json!({"trackIndex": 0}))
        .await;
    let clips = listed.result.unwrap();
    assert_eq!(clips.as_array().unwrap().len(), 2);
    assert_eq!(clips[0]["outFrame"], 39);
    assert_eq!(clips[1]["inFrame"], 40);

    let left_id = split_result["leftClipId"].as_str().unwrap().to_string();
    client
        .call(
            "filter.add",
            json!({"clipId": left_id, "mltService": "qtcrop", "properties": {"rect": "0 0 10 10"}}),
        )
        .await;
    client
        .call(
            "filter.add",
            json!({"clipId": left_id, "mltService": "brightness", "properties": {"level": 0.5}}),
        )
        .await;
    let filters = client.call("filter.list", json!({"clipId": left_id})).await;
    assert!(filters.error.is_none(), "filter.list: {:?}", filters.error);
    assert_eq!(filters.result.as_ref().unwrap().as_array().unwrap().len(), 2);

    let reorder = client
        .call(
            "filter.reorder",
            json!({"clipId": left_id, "filterIndex": 0, "newIndex": 1}),
        )
        .await;
    assert!(reorder.error.is_none(), "filter.reorder: {:?}", reorder.error);

    client
        .call(
            "filter.addKeyframe",
            json!({
                "clipId": left_id,
                "filterIndex": 0,
                "property": "level",
                "position": 10,
                "value": 0.2,
                "interpolation": "smooth",
            }),
        )
        .await;
    let kfs = client
        .call(
            "filter.listKeyframes",
            json!({"clipId": left_id, "filterIndex": 0, "property": "level"}),
        )
        .await;
    assert!(kfs.error.is_none(), "filter.listKeyframes: {:?}", kfs.error);
    let kf_arr = kfs.result.unwrap();
    assert_eq!(kf_arr.as_array().unwrap().len(), 1);
    assert_eq!(kf_arr[0]["interpolation"], "smooth");

    let rm_kf = client
        .call(
            "filter.removeKeyframe",
            json!({"clipId": left_id, "filterIndex": 0, "property": "level", "position": 10}),
        )
        .await;
    assert!(rm_kf.error.is_none(), "filter.removeKeyframe: {:?}", rm_kf.error);

    let rm = client
        .call("filter.remove", json!({"clipId": left_id, "filterIndex": 0}))
        .await;
    assert!(rm.error.is_none(), "filter.remove: {:?}", rm.error);
    let filters = client.call("filter.list", json!({"clipId": left_id})).await;
    assert_eq!(filters.result.unwrap().as_array().unwrap().len(), 1);
}

/// Proof for testing-plan.md Phase 3's `notes.*` row: "confirm notes.changed
/// notification fires on a second, concurrently-connected session" -- same
/// pattern as `edit_add_track_fans_out_to_other_client_on_same_project`/
/// `playlist_append_fans_out_to_other_client_on_same_project`, applied to
/// `notes.setText`. Also confirms the underlying text is actually shared
/// project state (client B reads back client A's write), not just that a
/// notification happened to arrive.
#[tokio::test]
async fn notes_set_text_fans_out_to_other_client_on_same_project() {
    let path = start_server("notes-fanout", TOKEN).await;

    let mut client_a = Client::connect(&path).await;
    client_a.call("sap.hello", json!({"token": TOKEN})).await;
    client_a.call("project.select", json!({"projectId": "shared-notes-project"})).await;

    let mut client_b = Client::connect(&path).await;
    client_b.call("sap.hello", json!({"token": TOKEN})).await;
    client_b.call("project.select", json!({"projectId": "shared-notes-project"})).await;

    let set = client_a.call("notes.setText", json!({"text": "hello from A"})).await;
    assert!(set.error.is_none(), "client A's notes.setText should succeed: {:?}", set.error);

    // Client B never called notes.setText itself -- it must still see the
    // change via an unsolicited notes.changed notification.
    let notification = client_b
        .recv_notification_timeout(Duration::from_secs(2))
        .await
        .expect("client B should receive a fan-out notes.changed notification");
    assert_eq!(notification.method, "notes.changed");
    assert_eq!(notification.params["reason"], "setText");

    // And the underlying state really is shared, not just the notification.
    let got = client_b.call("notes.getText", json!({})).await;
    assert!(got.error.is_none(), "client B's notes.getText should succeed: {:?}", got.error);
    assert_eq!(got.result.unwrap()["text"], "hello from A");
}

/// Targeted proof for 11-e2e-scenario-tests.md's Phase B step 4: "the
/// shared, single linear undo stack" means `project.undo()` issued from one
/// session can undo a *different* session's most recent edit, not
/// necessarily the caller's own last edit -- there is no per-session undo
/// scoping. Honesty note (same caveat `mlt_backend.rs`/`backend.rs`
/// document elsewhere): `project_undo` here is a plain shared depth
/// counter, not real command-level timeline rewind, so this proves the
/// stack's *sharedness and strict LIFO ordering across sessions* -- exactly
/// the semantic the doc asks to confirm given that stub, not full content-
/// level undo/redo.
#[tokio::test]
async fn project_undo_from_one_session_can_undo_a_different_sessions_most_recent_edit() {
    let path = start_server("shared-undo", TOKEN).await;

    let mut agent1 = Client::connect(&path).await;
    agent1.call("sap.hello", json!({"token": TOKEN})).await;
    agent1.call("project.select", json!({"projectId": "shared-undo-project"})).await;

    let mut agent2 = Client::connect(&path).await;
    agent2.call("sap.hello", json!({"token": TOKEN})).await;
    agent2.call("project.select", json!({"projectId": "shared-undo-project"})).await;

    // Agent 1 edits first...
    let a1 = agent1.call("edit.addTrack", json!({"kind": "video"})).await;
    assert!(a1.error.is_none(), "agent1 edit.addTrack: {:?}", a1.error);
    // ...then Agent 2 makes the *most recent* edit on the shared project,
    // strictly after Agent 1's.
    let a2 = agent2.call("edit.addTrack", json!({"kind": "audio"})).await;
    assert!(a2.error.is_none(), "agent2 edit.addTrack: {:?}", a2.error);

    let before = agent1.call("project.getState", json!({})).await;
    let before_undo = before.result.as_ref().unwrap()["undoDepth"].as_u64().unwrap();
    assert_eq!(before_undo, 2, "both agents' edits should land on the same shared undo stack");

    // Agent 1 undoes -- per 05-multi-client-concurrency.md's accepted
    // single-linear-stack policy, this must be able to undo Agent 2's most
    // recent edit; there is no per-session undo scoping to stop it.
    let undo = agent1.call("project.undo", json!({})).await;
    assert!(undo.error.is_none(), "agent1's project.undo should succeed: {:?}", undo.error);

    // Agent 2 -- which never called undo itself -- must observe the shared
    // stack's new depth via its own project.getState, proving project state
    // (not just the connection) is what's shared.
    let after = agent2.call("project.getState", json!({})).await;
    let after_undo = after.result.as_ref().unwrap()["undoDepth"].as_u64().unwrap();
    let after_redo = after.result.as_ref().unwrap()["redoDepth"].as_u64().unwrap();
    assert_eq!(after_undo, before_undo - 1, "agent2 should observe the shared undoDepth decremented by agent1's undo");
    assert_eq!(after_redo, 1, "agent2 should observe redoDepth incremented by agent1's undo");

    // A second project.undo (still from agent1) must now undo Agent 1's own
    // remaining edit -- confirms strict LIFO order across both sessions'
    // edits, not e.g. silently stopping early or double-counting.
    let undo2 = agent1.call("project.undo", json!({})).await;
    assert!(undo2.error.is_none(), "agent1's second project.undo should succeed: {:?}", undo2.error);
    let final_state = agent2.call("project.getState", json!({})).await;
    assert_eq!(final_state.result.as_ref().unwrap()["undoDepth"].as_u64().unwrap(), 0);

    // The shared stack is now exhausted -- a further undo must fail, not
    // silently no-op or go negative.
    let too_many = agent1.call("project.undo", json!({})).await;
    assert!(too_many.error.is_some(), "undo past the bottom of the shared stack must fail, not silently succeed");
}
