//! **Phase 11.** Closes a claim that had been carried over, untested,
//! since at least phase 7's recheck list ("Any `ContentBlock` variant
//! (image/audio/resource) plumbing gaps in `session/prompt` -- acpx
//! claims to forward verbatim; confirm no accidental transformation")
//! and repeated again in phases 8/9/10 without ever being acted on.
//! Phase 10 found a real gap hiding behind exactly this kind of
//! previously-untested "acpx forwards it verbatim" claim (`terminal/
//! output`'s missing `truncated` field, discovered only by diffing
//! against the real schema and adding assertions -- not by re-reading
//! the code and reasoning it must already be fine). This test applies
//! the same discipline to `session/prompt`'s `ContentBlock` array and
//! `_meta` passthrough specifically.
//!
//! Real `ContentBlock` variant field lists (from `agentclientprotocol/
//! agent-client-protocol`'s `schema/v1/schema.json`, fetched directly in
//! phase 9): `text` (`text`, `annotations?`, `_meta?`), `image` (`data`,
//! `mimeType`, `uri?`, `annotations?`, `_meta?`), `audio` (`data`,
//! `mimeType`, `annotations?`, `_meta?`), `resource_link` (`uri`,
//! `name`, `mimeType?`, `title?`, `description?`, `size?`,
//! `annotations?`, `_meta?`), `resource` (`resource`, `annotations?`,
//! `_meta?`). All five appear in one single `session/prompt` call below,
//! plus a top-level `_meta` on the request itself -- `PromptRequest`'s
//! own real schema is `{sessionId, prompt, _meta?}`.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;
use std::time::Duration;

/// Answers `session/new` normally. Every other request's raw line is
/// appended verbatim to `capture_path` (so the test can deserialize
/// exactly what acpx put on the wire and compare it structurally to what
/// the client originally sent), then answered with a generic
/// `{"ok": true}` so `dispatch` itself doesn't hang.
fn stand_in_capture_backend_script(capture_path: &str) -> String {
    format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  else
    echo "$line" >> {capture_path}
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#
    )
}

fn stand_in_capture_backend_spec(capture_path: &str) -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            stand_in_capture_backend_script(capture_path),
        ],
    )
}

fn unique_capture_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "acpx-prompt-content-passthrough-test-{label}-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ))
}

/// Reads back the first captured line whose "method" is "session/prompt"
/// -- not simply the first line in the file. ensure_backend_initialized
/// sends its own initialize/authenticate handshake requests to the
/// backend during session/new's own dispatch (also captured here, since
/// this stand-in's capture branch fires for anything that isn't
/// literally "method":"session/new"), so those land in the file before
/// the real session/prompt line this test actually cares about.
async fn wait_for_prompt_capture(capture_path: &std::path::Path) -> serde_json::Value {
    for _ in 0..100 {
        if let Ok(contents) = tokio::fs::read_to_string(capture_path).await {
            for line in contents.lines() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    if value["method"] == json!("session/prompt") {
                        return value;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("backend never captured a session/prompt request at {capture_path:?}");
}

#[tokio::test]
async fn session_prompt_forwards_every_content_block_variant_and_meta_verbatim() {
    let capture_path = unique_capture_path("content-blocks");
    let mut router = Router::new("stand-in-agent");
    router.register_agent(
        "stand-in-agent",
        stand_in_capture_backend_spec(&capture_path.to_string_lossy()),
    );

    let new_response = router
        .dispatch(
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // One of every real ContentBlock variant, plus annotations/_meta on
    // individual blocks and a top-level PromptRequest._meta -- exactly
    // the shape a real spec-compliant client could send.
    let prompt_blocks = json!([
        {
            "type": "text",
            "text": "check this image and audio clip against the linked resource",
            "annotations": {"audience": ["user"], "priority": 0.5},
            "_meta": {"source": "editor-selection"}
        },
        {
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png",
            "uri": "file:///tmp/screenshot.png"
        },
        {
            "type": "audio",
            "data": "d29ybGQ=",
            "mimeType": "audio/wav"
        },
        {
            "type": "resource_link",
            "uri": "file:///tmp/notes.md",
            "name": "notes.md",
            "mimeType": "text/markdown",
            "title": "Notes",
            "description": "some notes",
            "size": 1234
        },
        {
            "type": "resource",
            "resource": {
                "uri": "file:///tmp/embedded.txt",
                "mimeType": "text/plain",
                "text": "embedded content"
            }
        }
    ]);
    let original_params = json!({
        "sessionId": gateway_id,
        "prompt": prompt_blocks,
        "_meta": {"acpxTestMarker": "phase-11", "nested": {"a": [1, 2, 3]}}
    });

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": original_params.clone()
    });
    let response = router
        .dispatch(prompt_request)
        .await
        .expect("session/prompt");
    assert_eq!(response["result"]["ok"], json!(true));

    let captured_request = wait_for_prompt_capture(&capture_path).await;
    let captured_params = &captured_request["params"];

    // sessionId is the one field the router is *supposed* to rewrite
    // (gateway id -> backend id) -- everything else must be untouched.
    assert_eq!(captured_params["sessionId"], json!("backend-abc"));
    assert_eq!(
        captured_params["prompt"], prompt_blocks,
        "ContentBlock array must reach the backend byte-for-byte identical, \
         including every variant's own type-specific fields"
    );
    assert_eq!(
        captured_params["_meta"], original_params["_meta"],
        "PromptRequest._meta must be forwarded verbatim, untouched"
    );
    // Per-block _meta/annotations survive as part of the deep-equal
    // `prompt` comparison above already, but assert on them individually
    // too so a future regression that only corrupts a nested field (not
    // the whole array) fails with a more specific message.
    assert_eq!(
        captured_params["prompt"][0]["_meta"],
        json!({"source": "editor-selection"})
    );
    assert_eq!(
        captured_params["prompt"][0]["annotations"]["priority"],
        json!(0.5)
    );

    let _ = std::fs::remove_file(&capture_path);
}
