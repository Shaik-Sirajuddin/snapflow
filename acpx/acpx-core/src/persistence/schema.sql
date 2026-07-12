-- acpx-core sqlite schema. Single source of truth (loaded via
-- include_str!), per 03-crate-and-folder-layout.md. Migration runner choice
-- deferred to Phase 2.

CREATE TABLE IF NOT EXISTS sessions (
    gateway_session_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    backend_session_id TEXT NOT NULL,
    profile_name TEXT,
    created_at TEXT NOT NULL,
    closed_at TEXT
);

CREATE TABLE IF NOT EXISTS transcripts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gateway_session_id TEXT NOT NULL REFERENCES sessions(gateway_session_id),
    direction TEXT NOT NULL, -- 'client_to_agent' | 'agent_to_client'
    payload TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_transcripts_session
    ON transcripts (gateway_session_id);
