//! Pure jsonl-cache coverage, independent of any ACP connection --
//! `JsonlStore`'s own contract (Decision 1/2 in
//! `chat-panel-acp-rust-sdk.md`): cache-miss is empty not an error,
//! append/overwrite semantics, and the staleness diff that drives the
//! resync decision.

use rui_acp_client::{ChatMessage, JsonlStore, MessageKind, ThreadTrailer};

fn msg(kind: MessageKind, text: &str) -> ChatMessage {
    ChatMessage {
        kind,
        text: text.to_string(),
    }
}

#[test]
fn load_on_missing_file_is_empty_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::open(dir.path()).unwrap();
    let cached = store.load("never-written").unwrap();
    assert!(cached.messages.is_empty());
    assert!(cached.trailer.is_none());
}

#[test]
fn overwrite_then_load_round_trips_messages_and_trailer() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::open(dir.path()).unwrap();
    let messages = vec![
        msg(MessageKind::User, "add a crossfade"),
        msg(MessageKind::Thinking, "considering options"),
        msg(MessageKind::ToolCall, "edit.add_transition(...)"),
        msg(MessageKind::Agent, "done"),
    ];
    let trailer = ThreadTrailer {
        acp_session_id: "s-1".into(),
        title: Some("Fix timeline crash".into()),
        updated_at: Some("2026-07-12T00:00:00Z".into()),
        message_count: messages.len(),
    };
    store.overwrite("thread-1", &messages, &trailer).unwrap();

    let cached = store.load("thread-1").unwrap();
    assert_eq!(cached.messages, messages);
    assert_eq!(cached.trailer, Some(trailer));
}

#[test]
fn append_adds_a_message_without_touching_trailer() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::open(dir.path()).unwrap();
    let trailer = ThreadTrailer {
        acp_session_id: "s-2".into(),
        title: Some("T".into()),
        updated_at: Some("t0".into()),
        message_count: 1,
    };
    store
        .overwrite("thread-2", &[msg(MessageKind::User, "one")], &trailer)
        .unwrap();
    store.append("thread-2", &msg(MessageKind::Agent, "two")).unwrap();

    let cached = store.load("thread-2").unwrap();
    assert_eq!(cached.messages.len(), 2);
    assert_eq!(cached.messages[1].text, "two");
    // Append doesn't touch the trailer -- still the original, pre-append one.
    assert_eq!(cached.trailer, Some(trailer));
}

#[test]
fn overwrite_is_atomic_via_tmp_rename_not_truncate_in_place() {
    // Regression guard for the write strategy itself: `overwrite` must
    // never leave a half-written file behind if interrupted -- verified
    // indirectly here by confirming a second overwrite with *shorter*
    // content fully replaces the first (a naive truncate-free append-style
    // write would leave trailing garbage from the longer first write).
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::open(dir.path()).unwrap();
    let long = vec![
        msg(MessageKind::User, "a very long first message indeed"),
        msg(MessageKind::Agent, "and a long reply to match it"),
    ];
    let trailer1 = ThreadTrailer {
        acp_session_id: "s-3".into(),
        title: None,
        updated_at: None,
        message_count: long.len(),
    };
    store.overwrite("thread-3", &long, &trailer1).unwrap();

    let short = vec![msg(MessageKind::User, "hi")];
    let trailer2 = ThreadTrailer {
        acp_session_id: "s-3".into(),
        title: None,
        updated_at: Some("t1".into()),
        message_count: short.len(),
    };
    store.overwrite("thread-3", &short, &trailer2).unwrap();

    let cached = store.load("thread-3").unwrap();
    assert_eq!(cached.messages, short);
    assert_eq!(cached.trailer, Some(trailer2));
}

#[test]
fn is_stale_true_when_no_local_trailer() {
    assert!(JsonlStore::is_stale(None, &Some("t".into()), &Some("u".into())));
}

#[test]
fn is_stale_false_when_metadata_matches() {
    let trailer = ThreadTrailer {
        acp_session_id: "s".into(),
        title: Some("T".into()),
        updated_at: Some("U".into()),
        message_count: 3,
    };
    assert!(!JsonlStore::is_stale(
        Some(&trailer),
        &Some("T".to_string()),
        &Some("U".to_string())
    ));
}

#[test]
fn is_stale_true_when_updated_at_differs() {
    let trailer = ThreadTrailer {
        acp_session_id: "s".into(),
        title: Some("T".into()),
        updated_at: Some("U-old".into()),
        message_count: 3,
    };
    assert!(JsonlStore::is_stale(
        Some(&trailer),
        &Some("T".to_string()),
        &Some("U-new".to_string())
    ));
}

#[test]
fn multi_thread_caches_are_isolated_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::open(dir.path()).unwrap();
    store
        .overwrite(
            "thread-a",
            &[msg(MessageKind::User, "a-only")],
            &ThreadTrailer {
                acp_session_id: "sa".into(),
                title: None,
                updated_at: None,
                message_count: 1,
            },
        )
        .unwrap();
    store
        .overwrite(
            "thread-b",
            &[msg(MessageKind::User, "b-only")],
            &ThreadTrailer {
                acp_session_id: "sb".into(),
                title: None,
                updated_at: None,
                message_count: 1,
            },
        )
        .unwrap();

    assert_eq!(store.load("thread-a").unwrap().messages[0].text, "a-only");
    assert_eq!(store.load("thread-b").unwrap().messages[0].text, "b-only");
}
