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
