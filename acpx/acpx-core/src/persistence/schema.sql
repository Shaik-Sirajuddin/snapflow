-- acpx-core sqlite schema. Single source of truth (loaded via
-- include_str!), per 03-crate-and-folder-layout.md. Migration runner choice
-- deferred to Phase 2.

CREATE TABLE IF NOT EXISTS sessions (
    gateway_session_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    backend_session_id TEXT NOT NULL,
    profile_name TEXT,
    created_at TEXT NOT NULL,
    closed_at TEXT,
    -- **Phase C (`acpx-tenant-isolation`).** Added after the table already
    -- shipped, so `CREATE TABLE IF NOT EXISTS` alone never applies this to
    -- a pre-existing on-disk database -- see `store.rs`'s
    -- `migrate_tenant_id_column` for the idempotent `ALTER TABLE` that
    -- covers upgrades. `NOT NULL DEFAULT 'default'` here only governs
    -- brand-new databases created after this change.
    tenant_id TEXT NOT NULL DEFAULT 'default',
    cwd TEXT,
    recovery_params_json TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    recovery_method TEXT NOT NULL DEFAULT 'none',
    last_recovery_error TEXT,
    pinned INTEGER NOT NULL DEFAULT 0,
    created_at_unix_nanos INTEGER,
    last_activity_at_unix_nanos INTEGER
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
