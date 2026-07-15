//! Typed, thread-scoped conversation reducer.
//!
//! Raw ACP/ACPX JSON is normalized before it reaches this module. The reducer
//! owns stable merge semantics for streamed message chunks, tool updates, and
//! terminal output so Slint projections never need to infer protocol state.

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
                ..
            } => self.upsert_tool(tool_call_id, title, status, detail),
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
    ) {
        if let Some(TranscriptItem::Tool {
            title: stored_title,
            status: stored_status,
            detail: stored_detail,
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
            return;
        }
        self.items.push(TranscriptItem::Tool {
            tool_call_id,
            title: title.unwrap_or_default(),
            status,
            detail,
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
        });
        state.apply(ConversationEvent::ToolCall {
            thread_id: "thread-a".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            title: None,
            status: Some("completed".to_owned()),
            detail: Some("src/main.rs".to_owned()),
        });
        assert_eq!(state.items().len(), 1);
        assert!(matches!(
            &state.items()[0],
            TranscriptItem::Tool { title, status, detail, .. }
                if title == "Read file"
                    && status.as_deref() == Some("completed")
                    && detail.as_deref() == Some("src/main.rs")
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
