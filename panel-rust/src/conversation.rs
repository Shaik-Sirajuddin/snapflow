//! Typed, thread-scoped conversation reducer.
//!
//! Raw ACP/ACPX JSON is normalized before it reaches this module. The reducer
//! owns stable merge semantics for streamed message chunks, tool updates, and
//! terminal output so Slint projections never need to infer protocol state.
//!
//! [`rebuild_from_chat_messages`] is this module's one real production
//! entry point (Phase 2 step 3, chat-panel-production-ui/execution-
//! plan.md): `AgentBridge` keeps `protocol_types::ChatMessage` as its
//! raw, append-only, per-chunk feed (unchanged -- it is also the JSONL
//! cache's on-disk row format, and many existing tests/consumers
//! already depend on its exact per-chunk shape/count), and calls this
//! function to derive a *merged* [`ConversationState`] from that feed
//! for anything UI-facing. `ChatMessage` carries `kind`/`text`/`status`/
//! optional `id` (wire `messageId`/`toolCallId`) plus optional
//! `raw_input`/`raw_output` for tool payloads (forwarded into
//! `TranscriptItem::Tool` for Slint details). This function also owns
//! the synthetic-id-when-absent heuristic: consecutive Assistant/
//! Thought chunks with no real id share one synthetic id, reset when a
//! different event kind interrupts (ToolCall, User, end of history).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptItem {
    User {
        message_id: String,
        text: String,
    },
    Assistant {
        message_id: String,
        text: String,
        streaming: bool,
    },
    Thought {
        message_id: String,
        text: String,
        streaming: bool,
    },
    Tool {
        tool_call_id: String,
        title: String,
        status: Option<String>,
        detail: Option<String>,
        /// Pre-serialized JSON for Slint `MessageItem.raw-input/output`
        /// (live tool details). Empty/`None` when the wire had no payload.
        raw_input: Option<String>,
        raw_output: Option<String>,
    },
    Terminal {
        terminal_id: String,
        title: String,
        output: String,
        exit_code: Option<i32>,
    },
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEvent {
    UserMessage {
        thread_id: String,
        message_id: String,
        text: String,
    },
    AssistantChunk {
        thread_id: String,
        message_id: String,
        text: String,
        completed: bool,
    },
    ThoughtChunk {
        thread_id: String,
        message_id: String,
        text: String,
        completed: bool,
    },
    ToolCall {
        thread_id: String,
        tool_call_id: String,
        title: Option<String>,
        status: Option<String>,
        detail: Option<String>,
        raw_input: Option<String>,
        raw_output: Option<String>,
    },
    TerminalCreated {
        thread_id: String,
        terminal_id: String,
        title: String,
    },
    TerminalOutput {
        thread_id: String,
        terminal_id: String,
        text: String,
    },
    TerminalExited {
        thread_id: String,
        terminal_id: String,
        exit_code: i32,
    },
    Notice {
        thread_id: String,
        text: String,
    },
}

impl ConversationEvent {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::UserMessage { thread_id, .. }
            | Self::AssistantChunk { thread_id, .. }
            | Self::ThoughtChunk { thread_id, .. }
            | Self::ToolCall { thread_id, .. }
            | Self::TerminalCreated { thread_id, .. }
            | Self::TerminalOutput { thread_id, .. }
            | Self::TerminalExited { thread_id, .. }
            | Self::Notice { thread_id, .. } => thread_id,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConversationState {
    thread_id: String,
    items: Vec<TranscriptItem>,
}

impl ConversationState {
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            items: Vec::new(),
        }
    }

    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// Flips every currently-streaming `Assistant`/`Thought` item's
    /// `streaming` flag to `false`. Called on `AgentEvent::TurnEnded` --
    /// v1 ACP has no explicit "this is the final chunk" marker on
    /// `agent_message_chunk`/`agent_thought_chunk` themselves (only an
    /// RFD-status, v1-optional `messageId`, see agentclientprotocol.com/
    /// rfds/message-id), so turn-end is this reducer's only definitive
    /// "nothing more will be appended to this message" signal.
    pub fn mark_all_streaming_completed(&mut self) {
        for item in &mut self.items {
            match item {
                TranscriptItem::Assistant { streaming, .. }
                | TranscriptItem::Thought { streaming, .. } => *streaming = false,
                _ => {}
            }
        }
    }

    /// Returns false for an event belonging to another thread. This makes a
    /// shared gateway event stream safe to fan out through per-thread states.
    pub fn apply(&mut self, event: ConversationEvent) -> bool {
        if event.thread_id() != self.thread_id {
            return false;
        }
        match event {
            ConversationEvent::UserMessage {
                message_id, text, ..
            } => self.upsert_user(message_id, text),
            ConversationEvent::AssistantChunk {
                message_id,
                text,
                completed,
                ..
            } => self.merge_text(message_id, text, completed, false),
            ConversationEvent::ThoughtChunk {
                message_id,
                text,
                completed,
                ..
            } => self.merge_text(message_id, text, completed, true),
            ConversationEvent::ToolCall {
                tool_call_id,
                title,
                status,
                detail,
                raw_input,
                raw_output,
                ..
            } => self.upsert_tool(tool_call_id, title, status, detail, raw_input, raw_output),
            ConversationEvent::TerminalCreated {
                terminal_id, title, ..
            } => self.upsert_terminal(terminal_id, Some(title), None, None),
            ConversationEvent::TerminalOutput {
                terminal_id, text, ..
            } => self.upsert_terminal(terminal_id, None, Some(text), None),
            ConversationEvent::TerminalExited {
                terminal_id,
                exit_code,
                ..
            } => self.upsert_terminal(terminal_id, None, None, Some(exit_code)),
            ConversationEvent::Notice { text, .. } => {
                self.items.push(TranscriptItem::Notice { text })
            }
        }
        true
    }

    fn upsert_user(&mut self, message_id: String, text: String) {
        if let Some(TranscriptItem::User {
            text: stored_text, ..
        }) = self.items.iter_mut().find(
            |item| matches!(item, TranscriptItem::User { message_id: id, .. } if id == &message_id),
        ) {
            *stored_text = text;
        } else {
            self.items.push(TranscriptItem::User { message_id, text });
        }
    }

    fn merge_text(&mut self, message_id: String, text: String, completed: bool, thought: bool) {
        for item in &mut self.items {
            match item {
                TranscriptItem::Assistant {
                    message_id: id,
                    text: stored_text,
                    streaming,
                } if !thought && id == &message_id => {
                    stored_text.push_str(&text);
                    *streaming = !completed;
                    return;
                }
                TranscriptItem::Thought {
                    message_id: id,
                    text: stored_text,
                    streaming,
                } if thought && id == &message_id => {
                    stored_text.push_str(&text);
                    *streaming = !completed;
                    return;
                }
                _ => {}
            }
        }
        if thought {
            self.items.push(TranscriptItem::Thought {
                message_id,
                text,
                streaming: !completed,
            });
        } else {
            self.items.push(TranscriptItem::Assistant {
                message_id,
                text,
                streaming: !completed,
            });
        }
    }

    fn upsert_tool(
        &mut self,
        tool_call_id: String,
        title: Option<String>,
        status: Option<String>,
        detail: Option<String>,
        raw_input: Option<String>,
        raw_output: Option<String>,
    ) {
        if let Some(TranscriptItem::Tool {
            title: stored_title,
            status: stored_status,
            detail: stored_detail,
            raw_input: stored_raw_input,
            raw_output: stored_raw_output,
            ..
        }) = self.items.iter_mut().find(|item| {
            matches!(item, TranscriptItem::Tool { tool_call_id: id, .. } if id == &tool_call_id)
        }) {
            if let Some(title) = title {
                *stored_title = title;
            }
            if status.is_some() {
                *stored_status = status;
            }
            if detail.is_some() {
                *stored_detail = detail;
            }
            // Later tool_call_update wins when it carries a payload; keep
            // prior values when the update omits them.
            if raw_input.is_some() {
                *stored_raw_input = raw_input;
            }
            if raw_output.is_some() {
                *stored_raw_output = raw_output;
            }
            return;
        }
        self.items.push(TranscriptItem::Tool {
            tool_call_id,
            title: title.unwrap_or_default(),
            status,
            detail,
            raw_input,
            raw_output,
        });
    }

    fn upsert_terminal(
        &mut self,
        terminal_id: String,
        title: Option<String>,
        output: Option<String>,
        exit_code: Option<i32>,
    ) {
        if let Some(TranscriptItem::Terminal {
            title: stored_title,
            output: stored_output,
            exit_code: stored_exit_code,
            ..
        }) = self.items.iter_mut().find(|item| {
            matches!(item, TranscriptItem::Terminal { terminal_id: id, .. } if id == &terminal_id)
        }) {
            if let Some(title) = title {
                *stored_title = title;
            }
            if let Some(output) = output {
                stored_output.push_str(&output);
            }
            if exit_code.is_some() {
                *stored_exit_code = exit_code;
            }
            return;
        }
        self.items.push(TranscriptItem::Terminal {
            terminal_id,
            title: title.unwrap_or_else(|| "Terminal".to_owned()),
            output: output.unwrap_or_default(),
            exit_code,
        });
    }
}

/// See this module's own doc comment. Pure function -- always rebuilds
/// a fresh [`ConversationState`] from the full ordered `history` slice
/// rather than mutating one incrementally, so a caller (`AgentBridge`)
/// gets a correct-by-construction merged view on every call with no
/// risk of incremental-merge state drifting out of sync with the raw
/// feed (e.g. after a jsonl-cache reload replaces `history` wholesale).
/// `thread_id` is only needed because [`ConversationEvent`]/
/// [`ConversationState::apply`] carry one for their own cross-thread
/// safety check (irrelevant here, since every event this function
/// constructs is for the one `history` it was given) -- any non-empty
/// string works; callers typically pass the real thread id for
/// debuggability.
pub fn rebuild_from_chat_messages(
    thread_id: &str,
    history: &[crate::protocol_types::ChatMessage],
) -> ConversationState {
    use crate::protocol_types::MessageKind as ChatKind;

    let mut state = ConversationState::new(thread_id);
    // The kind + id of the currently-open synthetic streaming run, if
    // any -- `None` means the next Assistant/Thought chunk with no real
    // id starts a fresh one. Cleared by any interruption (ToolCall,
    // User message) or when a chunk arrives with a *real* id (which
    // needs no synthetic fallback of its own).
    let mut open_run: Option<(ChatKind, String)> = None;
    let mut synthetic_counter: u64 = 0;
    let next_synthetic = |synthetic_counter: &mut u64| {
        *synthetic_counter += 1;
        format!("synthetic-{synthetic_counter}")
    };

    for msg in history {
        match msg.kind {
            ChatKind::User => {
                open_run = None;
                let message_id = msg
                    .id
                    .clone()
                    .unwrap_or_else(|| next_synthetic(&mut synthetic_counter));
                state.apply(ConversationEvent::UserMessage {
                    thread_id: thread_id.to_string(),
                    message_id,
                    text: msg.text.clone(),
                });
            }
            ChatKind::Agent | ChatKind::Thinking => {
                let message_id = match &msg.id {
                    Some(real_id) => {
                        open_run = Some((msg.kind.clone(), real_id.clone()));
                        real_id.clone()
                    }
                    None => match &open_run {
                        Some((kind, id)) if *kind == msg.kind => id.clone(),
                        _ => {
                            let id = next_synthetic(&mut synthetic_counter);
                            open_run = Some((msg.kind.clone(), id.clone()));
                            id
                        }
                    },
                };
                let event = if msg.kind == ChatKind::Thinking {
                    ConversationEvent::ThoughtChunk {
                        thread_id: thread_id.to_string(),
                        message_id,
                        text: msg.text.clone(),
                        // See this function's doc comment: `streaming`
                        // isn't wired to any Slint-visible field yet in
                        // this version, so every rebuild treats the
                        // whole history as fully resolved rather than
                        // tracking genuine mid-turn "still streaming"
                        // state (which `history` alone cannot express --
                        // it has no turn-boundary markers of its own).
                        completed: true,
                    }
                } else {
                    ConversationEvent::AssistantChunk {
                        thread_id: thread_id.to_string(),
                        message_id,
                        text: msg.text.clone(),
                        completed: true,
                    }
                };
                state.apply(event);
            }
            ChatKind::ToolCall => {
                open_run = None;
                let tool_call_id = msg
                    .id
                    .clone()
                    .unwrap_or_else(|| next_synthetic(&mut synthetic_counter));
                state.apply(ConversationEvent::ToolCall {
                    thread_id: thread_id.to_string(),
                    tool_call_id,
                    title: (!msg.text.is_empty()).then(|| msg.text.clone()),
                    status: msg.status.clone(),
                    detail: None,
                    raw_input: msg.raw_input.as_ref().map(|v| v.to_string()),
                    raw_output: msg.raw_output.as_ref().map(|v| v.to_string()),
                });
            }
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_merge_by_message_id_and_do_not_cross_threads() {
        let mut state = ConversationState::new("thread-a");
        assert!(state.apply(ConversationEvent::AssistantChunk {
            thread_id: "thread-a".to_owned(),
            message_id: "message-1".to_owned(),
            text: "Hello".to_owned(),
            completed: false,
        }));
        assert!(state.apply(ConversationEvent::AssistantChunk {
            thread_id: "thread-a".to_owned(),
            message_id: "message-1".to_owned(),
            text: " world".to_owned(),
            completed: true,
        }));
        assert!(!state.apply(ConversationEvent::AssistantChunk {
            thread_id: "thread-b".to_owned(),
            message_id: "message-1".to_owned(),
            text: " ignored".to_owned(),
            completed: true,
        }));
        assert_eq!(
            state.items(),
            &[TranscriptItem::Assistant {
                message_id: "message-1".to_owned(),
                text: "Hello world".to_owned(),
                streaming: false,
            }]
        );
    }

    #[test]
    fn tool_updates_merge_by_tool_call_id() {
        let mut state = ConversationState::new("thread-a");
        state.apply(ConversationEvent::ToolCall {
            thread_id: "thread-a".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            title: Some("Read file".to_owned()),
            status: Some("in_progress".to_owned()),
            detail: None,
            raw_input: Some(r#"{"path":"src/main.rs"}"#.to_owned()),
            raw_output: None,
        });
        state.apply(ConversationEvent::ToolCall {
            thread_id: "thread-a".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            title: None,
            status: Some("completed".to_owned()),
            detail: Some("src/main.rs".to_owned()),
            raw_input: None,
            raw_output: Some(r#"{"ok":true}"#.to_owned()),
        });
        assert_eq!(state.items().len(), 1);
        assert!(matches!(
            &state.items()[0],
            TranscriptItem::Tool {
                title,
                status,
                detail,
                raw_input,
                raw_output,
                ..
            } if title == "Read file"
                && status.as_deref() == Some("completed")
                && detail.as_deref() == Some("src/main.rs")
                && raw_input.as_deref() == Some(r#"{"path":"src/main.rs"}"#)
                && raw_output.as_deref() == Some(r#"{"ok":true}"#)
        ));
    }

    #[test]
    fn terminal_deltas_merge_by_terminal_id_before_and_after_create() {
        let mut state = ConversationState::new("thread-a");
        state.apply(ConversationEvent::TerminalOutput {
            thread_id: "thread-a".to_owned(),
            terminal_id: "terminal-1".to_owned(),
            text: "building\n".to_owned(),
        });
        state.apply(ConversationEvent::TerminalCreated {
            thread_id: "thread-a".to_owned(),
            terminal_id: "terminal-1".to_owned(),
            title: "cargo test".to_owned(),
        });
        state.apply(ConversationEvent::TerminalExited {
            thread_id: "thread-a".to_owned(),
            terminal_id: "terminal-1".to_owned(),
            exit_code: 0,
        });
        assert!(matches!(
            &state.items()[0],
            TranscriptItem::Terminal { title, output, exit_code, .. }
                if title == "cargo test" && output == "building\n" && *exit_code == Some(0)
        ));
    }
}
