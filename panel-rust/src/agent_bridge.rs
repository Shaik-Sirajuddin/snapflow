//! Bridges `rui-acp-client`'s async, per-thread ACP connections into
//! `panel-rust`'s single-OS-thread Slint world.
//!
//! Threading model (see `lib.rs` module docs): Slint/Qt must stay on one
//! OS thread. This module owns a background multi-thread tokio runtime
//! whose worker threads run entirely on their own, and *never* touch
//! Slint state directly. The only channel back to the UI thread is
//! `Mutex<VecDeque<BridgeEvent>>`, drained by [`AgentBridge::poll`] --
//! called periodically from a Qt timer via `panel_rust_poll`.
//!
//! ## JSON persistence (jsonl cache) and live reload
//!
//! Backed by [`rui_acp_client::JsonlStore`] -- one `<thread_id>.jsonl`
//! file per thread under the cache dir resolved by
//! [`resolve_cache_dir`].
//!
//! - **Cold start (renders smoothly from disk):** each thread's history
//!   is seeded from its jsonl file *before* the live agent connection is
//!   even spawned (see the `new_with_agent_cmd_and_cache_dir` loop
//!   below), so the very first render (`panel_rust_create` ->
//!   `bridge.history(0)`) shows cached scrollback immediately, with zero
//!   dependency on a subprocess round trip having completed.
//! - **No conflict when json content varies:** the seeded messages are
//!   plain `Vec<ChatMessage>` appended in file order, whatever mix of
//!   `MessageKind`s they happen to contain -- there is no schema
//!   reconciliation step, so a cache file from a longer or differently
//!   shaped prior run loads exactly as written, and the UI thread only
//!   ever reads a fully-formed snapshot through the same
//!   `Mutex<Vec<ChatMessage>>` the live path appends to (never a
//!   torn/partial write -- see `ThreadSlot::history`).
//! - **Async live reload:** as the bound agent streams new messages in
//!   (on a background runtime thread), each is pushed onto that same
//!   `history` mutex *and* appended to the jsonl file, in that order.
//!   Because appends never truncate or reorder what's already there, a
//!   live message arriving after a cache-seeded render composes cleanly
//!   on top of it -- the UI thread (via `poll` + `history`) never
//!   observes a state that mixes half of one write with half of another.
//! - **Trailer refresh:** on each `AgentEvent::TurnEnded`, the trailer is
//!   rewritten (`JsonlStore::overwrite`, with the full in-memory history
//!   as of that turn boundary) so the cache file's metadata (session id,
//!   message count) reflects true state -- deliberately not on every
//!   streamed message chunk, to avoid rewriting the whole file on every
//!   token.
//! - **Not implemented (deliberate scope boundary):** the full
//!   `session/list`-diff resync sequence from
//!   `chat-panel-acp-rust-sdk.md` Decision 2. `rui-mock-agent` (the only
//!   agent available to test against in this repo) does not persist
//!   sessions server-side across process restarts, so treating a fresh
//!   agent connection as source-of-truth on cold start would erase the
//!   jsonl cache instead of protecting it. jsonl is source-of-truth for
//!   pre-restart scrollback; the live agent connection is source-of-truth
//!   for anything from this run forward. Revisit once a real ACP agent
//!   with durable server-side session storage exists to validate
//!   against.

use rui_acp_client::{
    spawn_thread, AcpAgent, AgentEvent, ChatMessage, JsonlStore, ThreadHandle, ThreadTrailer,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(thiserror::Error, Debug)]
pub enum BridgeError {
    #[error("failed to start background async runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("jsonl cache error: {0}")]
    Cache(#[source] rui_acp_client::CacheError),
    #[error("invalid agent command {cmd:?}: {reason}")]
    Agent { cmd: String, reason: String },
}

/// One agent-bridge event, tagged with which UI thread index it belongs
/// to. `panel-rust`'s `PanelSingleton::apply_bridge_events` matches on
/// `event` for thread-status transitions and, for `Message`, re-reads
/// `AgentBridge::history` rather than trusting text carried here --
/// single source of truth is the mutex-guarded history, not the event.
pub struct BridgeEvent {
    pub thread_index: usize,
    pub event: AgentEvent,
}

/// One UI thread's state: its live agent handle, its jsonl-backed
/// scrollback (seeded at cold start, appended to live), and the ACP
/// session id once `open_session` resolves (used to fill the trailer).
struct ThreadSlot {
    thread_id: String,
    handle: Arc<ThreadHandle>,
    history: Mutex<Vec<ChatMessage>>,
    acp_session_id: Mutex<Option<String>>,
}

/// Owns the background runtime, the per-thread agent connections, the
/// jsonl cache, and the event queue the UI thread drains via `poll`.
pub struct AgentBridge {
    runtime: tokio::runtime::Runtime,
    slots: Vec<Arc<ThreadSlot>>,
    events: Arc<Mutex<VecDeque<BridgeEvent>>>,
    #[allow(dead_code)] // kept alive for its Drop / for future direct use
    store: Option<JsonlStore>,
}

/// Turns a UI thread display name into a filesystem-safe, stable jsonl
/// cache key -- lowercased, non-alphanumerics collapsed to `-`. Stable
/// across runs as long as `THREAD_NAMES` (in `lib.rs`) doesn't change,
/// which is the v1 fixed-thread-list assumption documented there.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for ch in name.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Resolves the real agent subprocess command: `RUI_ACP_AGENT_CMD` env
/// override (production/packaging path -- a real ACP-compliant agent
/// binary), else the dev-checkout `rui-mock-agent` built alongside
/// `rui-acp-client` (only usable from a source checkout, never in a
/// packaged build -- matches this crate's own `CARGO_MANIFEST_DIR`).
pub fn resolve_agent_command() -> String {
    if let Ok(cmd) = std::env::var("RUI_ACP_AGENT_CMD") {
        return cmd;
    }
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .join("../rui-acp-client/target/debug/rui-mock-agent")
        .to_string_lossy()
        .into_owned()
}

/// Resolves the jsonl cache directory: `RUI_ACP_CACHE_DIR` env override,
/// else a dev-checkout fallback sibling to this crate.
pub fn resolve_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RUI_ACP_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.rui-thread-cache")
}

/// Opaque staleness token -- not a real RFC3339 timestamp (no chrono
/// dependency pulled in just for this), only ever compared for equality
/// against itself by a future resync check, per the module doc's
/// documented scope boundary.
fn now_token() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

impl AgentBridge {
    /// Production constructor: real agent command + real (dev-checkout)
    /// cache dir, both resolved via env-override-or-fallback.
    pub fn new(thread_names: &[&str]) -> Result<Self, BridgeError> {
        Self::new_with_agent_cmd_and_cache_dir(
            thread_names,
            resolve_agent_command(),
            Some(resolve_cache_dir()),
        )
    }

    /// Test/override constructor: caller-chosen agent command, no jsonl
    /// persistence (in-memory history only) -- what the existing Rust
    /// test suite used before this module had a cache dir parameter at
    /// all, kept working unchanged.
    pub fn new_with_agent_cmd(thread_names: &[&str], agent_cmd: String) -> Result<Self, BridgeError> {
        Self::new_with_agent_cmd_and_cache_dir(thread_names, agent_cmd, None)
    }

    /// The real constructor both of the above delegate to: caller-chosen
    /// agent command and, optionally, a jsonl cache directory. `None`
    /// disables persistence entirely (pure in-memory history, matching
    /// pre-persistence behavior) rather than silently picking a
    /// directory the caller didn't ask for.
    pub fn new_with_agent_cmd_and_cache_dir(
        thread_names: &[&str],
        agent_cmd: String,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(BridgeError::Runtime)?;

        let store = match cache_dir {
            Some(dir) => Some(JsonlStore::open(dir).map_err(BridgeError::Cache)?),
            None => None,
        };

        let events: Arc<Mutex<VecDeque<BridgeEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut slots = Vec::with_capacity(thread_names.len());

        // `spawn_thread` calls the free-function `tokio::spawn` internally,
        // which needs an active runtime context on this (calling) thread --
        // `enter()` provides that for the duration of this loop. The tasks
        // it schedules then run on the runtime's own worker threads for the
        // rest of the process's life, well past this guard's drop.
        let _guard = runtime.enter();
        for (idx, name) in thread_names.iter().enumerate() {
            let thread_id = slug(name);

            // Cold-start seed: read whatever this thread's jsonl file
            // already holds -- of any prior shape/length -- *before*
            // spawning the live connection below, so `history(idx)` is
            // immediately populated for the first render.
            let seeded = match &store {
                Some(s) => s.load(&thread_id).map_err(BridgeError::Cache)?.messages,
                None => Vec::new(),
            };

            let transport = AcpAgent::from_str(&agent_cmd).map_err(|e| BridgeError::Agent {
                cmd: agent_cmd.clone(),
                reason: e.to_string(),
            })?;
            let mut handle = spawn_thread(transport);
            let mut events_rx = handle.take_events();
            let handle = Arc::new(handle);

            let slot = Arc::new(ThreadSlot {
                thread_id: thread_id.clone(),
                handle: handle.clone(),
                history: Mutex::new(seeded),
                acp_session_id: Mutex::new(None),
            });
            slots.push(slot.clone());

            let events_out = events.clone();
            let store_for_task = store.clone();
            let slot_for_task = slot;
            let handle_for_task = handle;
            runtime.spawn(async move {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                match handle_for_task.open_session(cwd).await {
                    Ok(session_id) => {
                        *slot_for_task
                            .acp_session_id
                            .lock()
                            .expect("acp_session_id mutex poisoned") = Some(session_id);
                    }
                    Err(e) => {
                        events_out
                            .lock()
                            .expect("event queue mutex poisoned")
                            .push_back(BridgeEvent {
                                thread_index: idx,
                                event: AgentEvent::Error(format!("open_session failed: {e}")),
                            });
                        return;
                    }
                }

                while let Some(ev) = events_rx.recv().await {
                    match &ev {
                        AgentEvent::Message(msg) => {
                            slot_for_task
                                .history
                                .lock()
                                .expect("history mutex poisoned")
                                .push(msg.clone());
                            if let Some(store) = &store_for_task {
                                if let Err(e) = store.append(&slot_for_task.thread_id, msg) {
                                    eprintln!(
                                        "panel-rust: jsonl append failed for {}: {e}",
                                        slot_for_task.thread_id
                                    );
                                }
                            }
                        }
                        AgentEvent::TurnEnded(_) => {
                            if let Some(store) = &store_for_task {
                                let hist = slot_for_task
                                    .history
                                    .lock()
                                    .expect("history mutex poisoned")
                                    .clone();
                                let session_id = slot_for_task
                                    .acp_session_id
                                    .lock()
                                    .expect("acp_session_id mutex poisoned")
                                    .clone()
                                    .unwrap_or_default();
                                let trailer = ThreadTrailer {
                                    acp_session_id: session_id,
                                    title: Some(slot_for_task.thread_id.clone()),
                                    updated_at: Some(now_token()),
                                    message_count: hist.len(),
                                };
                                if let Err(e) =
                                    store.overwrite(&slot_for_task.thread_id, &hist, &trailer)
                                {
                                    eprintln!(
                                        "panel-rust: jsonl trailer overwrite failed for {}: {e}",
                                        slot_for_task.thread_id
                                    );
                                }
                            }
                        }
                        AgentEvent::Error(_) => {}
                    }
                    events_out
                        .lock()
                        .expect("event queue mutex poisoned")
                        .push_back(BridgeEvent {
                            thread_index: idx,
                            event: ev,
                        });
                }
            });
        }
        drop(_guard);

        Ok(AgentBridge {
            runtime,
            slots,
            events,
            store,
        })
    }

    /// Drains every event queued since the last call. Non-blocking, safe
    /// to call from the Slint/UI thread on a timer -- see `lib.rs`'s
    /// `panel_rust_poll`. By the time an event is visible here, any
    /// history mutation it implies has already been applied (see the
    /// forwarder task above), so callers can immediately follow up with
    /// `history(idx)` for a consistent view.
    pub fn poll(&self) -> Vec<BridgeEvent> {
        self.events
            .lock()
            .expect("event queue mutex poisoned")
            .drain(..)
            .collect()
    }

    /// Snapshot of a thread's full scrollback (jsonl-seeded entries plus
    /// anything streamed live since), in display order.
    pub fn history(&self, idx: usize) -> Vec<ChatMessage> {
        self.slots
            .get(idx)
            .map(|s| s.history.lock().expect("history mutex poisoned").clone())
            .unwrap_or_default()
    }

    /// Immediately (synchronously) records a locally-originated message
    /// (the user's own compose-box send) into both in-memory history and
    /// the jsonl cache, ahead of any network round trip -- so
    /// `history(idx)` reflects it the instant this returns, and a crash
    /// before the agent's reply arrives still leaves the user's own
    /// message durably cached.
    pub fn push_local(&self, idx: usize, msg: ChatMessage) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        slot.history
            .lock()
            .expect("history mutex poisoned")
            .push(msg.clone());
        if let Some(store) = &self.store {
            if let Err(e) = store.append(&slot.thread_id, &msg) {
                eprintln!("panel-rust: jsonl append failed for {}: {e}", slot.thread_id);
            }
        }
    }

    /// Fire-and-forget: dispatches `text` to the given thread's bound
    /// agent on the background runtime. Errors surface as a queued
    /// `AgentEvent::Error`, consistent with every other agent-originated
    /// event, rather than via a return value the (synchronous) caller
    /// couldn't usefully act on anyway.
    pub fn send_prompt(&self, idx: usize, text: String) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(e) = handle.send_prompt(text).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("send_prompt failed: {e}")),
                    });
            }
        });
    }
}

impl Drop for AgentBridge {
    fn drop(&mut self) {
        // Ask every actor to stop so its forwarder task's `events_rx.recv()`
        // returns `None` and unwinds cleanly, instead of relying purely on
        // the runtime's own shutdown-cancels-outstanding-tasks behavior.
        for slot in &self.slots {
            slot.handle.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rui_acp_client::MessageKind;

    fn mock_agent_cmd() -> String {
        env!("CARGO_MANIFEST_DIR")
            .to_string()
            .replace("panel-rust", "rui-acp-client")
            + "/target/debug/rui-mock-agent"
    }

    /// Cold-start persistence: a message written by one bridge instance
    /// is visible (without any live agent involvement) to a second bridge
    /// instance pointed at the same cache dir -- the "later async live
    /// reload" contract from a prior run's perspective.
    #[test]
    fn history_persists_across_bridge_restarts_via_jsonl_cache() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let names = ["Thread One"];

        {
            let bridge = AgentBridge::new_with_agent_cmd_and_cache_dir(
                &names,
                mock_agent_cmd(),
                Some(cache_dir.path().to_path_buf()),
            )
            .expect("first bridge");
            bridge.push_local(
                0,
                ChatMessage {
                    kind: MessageKind::User,
                    text: "hello from run one".into(),
                },
            );
            assert_eq!(bridge.history(0).len(), 1);
        }

        let bridge2 = AgentBridge::new_with_agent_cmd_and_cache_dir(
            &names,
            mock_agent_cmd(),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("second bridge");
        let history = bridge2.history(0);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "hello from run one");
        assert_eq!(history[0].kind, MessageKind::User);
    }

    /// No cross-thread bleed in the jsonl cache -- each thread's file is
    /// keyed by its own slug.
    #[test]
    fn distinct_threads_get_isolated_cache_files() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let names = ["Thread A", "Thread B"];
        let bridge = AgentBridge::new_with_agent_cmd_and_cache_dir(
            &names,
            mock_agent_cmd(),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge");
        bridge.push_local(
            0,
            ChatMessage {
                kind: MessageKind::User,
                text: "a-only".into(),
            },
        );
        bridge.push_local(
            1,
            ChatMessage {
                kind: MessageKind::User,
                text: "b-only".into(),
            },
        );
        assert_eq!(bridge.history(0)[0].text, "a-only");
        assert_eq!(bridge.history(1)[0].text, "b-only");

        let a_file = std::fs::read_to_string(cache_dir.path().join("thread-a.jsonl")).unwrap();
        let b_file = std::fs::read_to_string(cache_dir.path().join("thread-b.jsonl")).unwrap();
        assert!(a_file.contains("a-only"));
        assert!(b_file.contains("b-only"));
        assert!(!a_file.contains("b-only"));
        assert!(!b_file.contains("a-only"));
    }

    /// `new_with_agent_cmd` (no cache dir) keeps working in-memory-only,
    /// so the pre-persistence test suite / call sites are unaffected.
    #[test]
    fn no_cache_dir_means_no_jsonl_file_written() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        std::env::set_current_dir(&cache_dir).ok(); // harmless if it fails
        let names = ["Solo Thread"];
        let bridge =
            AgentBridge::new_with_agent_cmd(&names, mock_agent_cmd()).expect("bridge");
        bridge.push_local(
            0,
            ChatMessage {
                kind: MessageKind::User,
                text: "not persisted".into(),
            },
        );
        assert_eq!(bridge.history(0).len(), 1);
        assert!(!cache_dir.path().join("solo-thread.jsonl").exists());
    }

    #[test]
    fn slug_collapses_non_alphanumerics_and_lowercases() {
        assert_eq!(slug("Fix timeline crash"), "fix-timeline-crash");
        assert_eq!(slug("Export pipeline bug!"), "export-pipeline-bug");
    }

    /// End-to-end: a jsonl cache file seeded up front with a varied mix
    /// of message kinds (thinking/tool-call/user/agent, i.e. not just plain
    /// user/agent turns) renders immediately via `history(0)`, and once
    /// the live mock agent streams a real reply for a new prompt, the
    /// pre-seeded entries are neither lost nor reordered -- the live
    /// messages land strictly after them. This is the concrete
    /// "json loading renders smoothly, no conflict with later async live
    /// reload" contract this module's docs describe.
    #[test]
    fn varied_seeded_json_and_live_reload_compose_without_conflict() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let names = ["Fix timeline crash"];
        let thread_id = slug(names[0]);

        // Seed the cache directly (as if written by a prior run) with a
        // deliberately varied mix of message kinds, independent of this
        // bridge -- mirrors "content varies in json".
        let seed_store = JsonlStore::open(cache_dir.path()).expect("open store for seeding");
        let seeded_messages = vec![
            ChatMessage {
                kind: MessageKind::User,
                text: "add a crossfade".into(),
            },
            ChatMessage {
                kind: MessageKind::Thinking,
                text: "considering the timeline structure".into(),
            },
            ChatMessage {
                kind: MessageKind::ToolCall,
                text: "edit.add_transition(...)".into(),
            },
            ChatMessage {
                kind: MessageKind::Agent,
                text: "done, crossfade added".into(),
            },
        ];
        seed_store
            .overwrite(
                &thread_id,
                &seeded_messages,
                &ThreadTrailer {
                    acp_session_id: "prior-run-session".into(),
                    title: Some(thread_id.clone()),
                    updated_at: Some("unix:1".into()),
                    message_count: seeded_messages.len(),
                },
            )
            .expect("seed cache file");

        let bridge = AgentBridge::new_with_agent_cmd_and_cache_dir(
            &names,
            mock_agent_cmd(),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge");

        // Renders smoothly from disk immediately, before any live
        // connection work has necessarily completed.
        let initial = bridge.history(0);
        assert_eq!(initial, seeded_messages);

        // Drive one real live turn through the mock agent subprocess and
        // wait (bounded) for its events to land via poll().
        bridge.send_prompt(0, "second look".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_turn_ended = false;
        while std::time::Instant::now() < deadline && !saw_turn_ended {
            for ev in bridge.poll() {
                if let AgentEvent::TurnEnded(_) = ev.event {
                    saw_turn_ended = true;
                }
            }
            if !saw_turn_ended {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(saw_turn_ended, "timed out waiting for the mock agent's turn to end");

        let after = bridge.history(0);
        // The four pre-seeded, varied-kind messages are untouched and
        // still first, in original order.
        assert_eq!(&after[..4], &seeded_messages[..]);
        // The mock agent's reply (uppercased echo, per mock_agent.rs) is
        // appended strictly after them, not interleaved or overwriting.
        assert!(after.len() > 4);
        assert!(after.iter().skip(4).any(|m| m.text == "SECOND LOOK"));

        // And the on-disk file reflects the same merged, non-conflicting
        // view after the TurnEnded-triggered trailer overwrite.
        let reloaded = seed_store.load(&thread_id).expect("reload from disk");
        assert_eq!(&reloaded.messages[..4], &seeded_messages[..]);
        assert!(reloaded.messages.len() > 4);
    }
}
