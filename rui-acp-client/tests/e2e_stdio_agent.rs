//! Phase 2 + phase 3 coverage: a real subprocess ACP agent
//! (`rui-mock-agent`, `src/bin/mock_agent.rs`) spoken to over real stdio
//! pipes -- no in-process shortcuts. Per
//! `chat-panel-acp-rust-sdk.md` step 2 ("spawn a real backend agent over
//! stdio... validate session/new -> session/prompt -> session/update
//! streaming end to end") and step 3 (jsonl cache layer, validated against
//! that same real agent process).

use rui_acp_client::{ChatMessage, MessageKind};
use rui_acp_client::{AcpAgent, AgentEvent, JsonlStore, SessionClient, ThreadId, ThreadTrailer};
use std::str::FromStr;

fn agent_message(text: &str) -> ChatMessage {
    ChatMessage {
        kind: MessageKind::Agent,
        text: text.to_string(),
    }
}

fn mock_agent_transport() -> AcpAgent {
    let bin = env!("CARGO_BIN_EXE_rui-mock-agent");
    AcpAgent::from_str(bin).expect("valid mock-agent command")
}

/// Drains one prompt turn's events into (messages, stop_reason).
async fn drain_turn(thread: &mut rui_acp_client::ThreadHandle) -> (Vec<String>, String) {
    let mut texts = Vec::new();
    loop {
        match thread.events.recv().await.expect("event channel open") {
            AgentEvent::Message(m) => texts.push(m.text),
            AgentEvent::TurnEnded(reason) => return (texts, reason),
            AgentEvent::Error(e) => panic!("unexpected error event: {e}"),
        }
    }
}

#[tokio::test]
async fn subprocess_session_new_prompt_update_roundtrip() {
    let mut client = SessionClient::new();
    let thread = client.bind_thread(ThreadId("real-1".into()), mock_agent_transport());

    let session_id = thread.open_session("/tmp").await.expect("open_session");
    assert!(session_id.starts_with("mock-session-"));

    thread.send_prompt("add a crossfade").await.expect("send_prompt");
    let (texts, reason) = drain_turn(thread).await;

    assert_eq!(reason, "end_turn");
    // Mock agent streams: thought, tool-call, then the uppercased echo --
    // three distinct content updates per turn, per mock_agent.rs's `send_replay`.
    assert_eq!(texts.len(), 3);
    assert!(texts[0].contains("considering"));
    assert!(texts[1].contains("mock_tool"));
    assert_eq!(texts[2], "ADD A CROSSFADE");
}

#[tokio::test]
async fn subprocess_multi_turn_same_session() {
    let mut client = SessionClient::new();
    let thread = client.bind_thread(ThreadId("real-2".into()), mock_agent_transport());

    thread.open_session("/tmp").await.expect("open_session");

    thread.send_prompt("first").await.expect("prompt 1");
    let (t1, _) = drain_turn(thread).await;
    assert_eq!(t1[2], "FIRST");

    thread.send_prompt("second").await.expect("prompt 2");
    let (t2, _) = drain_turn(thread).await;
    assert_eq!(t2[2], "SECOND");
}

#[tokio::test]
async fn subprocess_multi_session_no_cross_contamination() {
    // Two threads, two independent subprocess agent connections, per
    // Decision 4's per-thread binding -- confirm no bleed between their
    // jsonl caches or event streams (chat-panel-acp-rust-sdk.md's phase 7
    // test-matrix requirement, scaled down to two same-adapter connections
    // since only one mock agent binary exists in this repo; the point --
    // isolation between concurrent connections -- is identical regardless
    // of whether the two adapters are the same binary or different ones).
    let mut client = SessionClient::new();
    let thread_a = client.bind_thread(ThreadId("iso-a".into()), mock_agent_transport());
    thread_a.open_session("/tmp").await.expect("open a");
    thread_a.send_prompt("alpha").await.expect("prompt a");
    let (a_texts, _) = drain_turn(thread_a).await;

    let thread_b = client.bind_thread(ThreadId("iso-b".into()), mock_agent_transport());
    thread_b.open_session("/tmp").await.expect("open b");
    thread_b.send_prompt("beta").await.expect("prompt b");
    let (b_texts, _) = drain_turn(thread_b).await;

    assert_eq!(a_texts[2], "ALPHA");
    assert_eq!(b_texts[2], "BETA");

    let cache_dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlStore::open(cache_dir.path()).expect("open store");
    let msg_a = agent_message(&a_texts[2]);
    let msg_b = agent_message(&b_texts[2]);
    store
        .overwrite(
            "iso-a",
            &[msg_a],
            &ThreadTrailer {
                acp_session_id: "s-a".into(),
                title: Some("A".into()),
                updated_at: Some("t1".into()),
                message_count: 1,
            },
        )
        .unwrap();
    store
        .overwrite(
            "iso-b",
            &[msg_b],
            &ThreadTrailer {
                acp_session_id: "s-b".into(),
                title: Some("B".into()),
                updated_at: Some("t1".into()),
                message_count: 1,
            },
        )
        .unwrap();

    let cached_a = store.load("iso-a").unwrap();
    let cached_b = store.load("iso-b").unwrap();
    assert_eq!(cached_a.messages[0].text, "ALPHA");
    assert_eq!(cached_b.messages[0].text, "BETA");
    assert_ne!(cached_a.trailer.unwrap().acp_session_id, cached_b.trailer.unwrap().acp_session_id);
}

/// File-load latency: cache-hit render (sync jsonl read) must be
/// meaningfully cheaper than a cache-miss resync (real subprocess
/// round-trip through `list_sessions`), per Decision 2's whole reason for
/// existing ("what makes 'one chat window open at a time' feel instant").
#[tokio::test]
async fn cache_hit_is_faster_than_subprocess_roundtrip() {
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlStore::open(cache_dir.path()).expect("open store");
    store
        .overwrite(
            "latency-thread",
            &[agent_message("cached reply")],
            &ThreadTrailer {
                acp_session_id: "s-x".into(),
                title: Some("X".into()),
                updated_at: Some("t1".into()),
                message_count: 1,
            },
        )
        .unwrap();

    let cache_start = std::time::Instant::now();
    let cached = store.load("latency-thread").unwrap();
    let cache_elapsed = cache_start.elapsed();
    assert_eq!(cached.messages[0].text, "cached reply");

    let mut client = SessionClient::new();
    let thread = client.bind_thread(ThreadId("latency".into()), mock_agent_transport());
    let remote_start = std::time::Instant::now();
    thread.list_sessions().await.expect("list_sessions");
    let remote_elapsed = remote_start.elapsed();

    assert!(
        cache_elapsed < remote_elapsed,
        "cache read ({cache_elapsed:?}) should be faster than a subprocess round trip ({remote_elapsed:?})"
    );
}
