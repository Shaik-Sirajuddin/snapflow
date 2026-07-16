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
    last_activity_at_unix_nanos INTEGER,
    bridge_session_id TEXT,
    bridge_model_alias TEXT,
    bridge_config_options_json TEXT
    ,
    -- `retention_administration` (`acpx-session-lifecycle`). Per-session
    -- idle-TTL override in whole seconds; NULL means "no override, use
    -- the deployment default". Added after the table already shipped,
    -- so `store.rs`'s `migrate_sessions_columns` also idempotently
    -- `ALTER TABLE`s this in for a pre-existing database.
    custom_idle_ttl_seconds INTEGER
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

CREATE TABLE IF NOT EXISTS agent_enablement (
    agent_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1))
);

CREATE TABLE IF NOT EXISTS custom_agents (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL,
    env_json TEXT NOT NULL,
    cwd TEXT
);

-- `acp-gateway-daemon`'s `durable_secret_and_configuration_store` item.
-- Runtime CRUD configuration (profiles/mcp_servers, both JSON-RPC-mutable
-- today) and key material now survive a restart when `ACPX_DB_PATH` is
-- set, closing the "in-memory only" gap. Providers stay provisioning-file
-- driven (see `acpx-server/src/provisioning.rs`) but get a best-effort
-- mirror here too so a restart doesn't require re-declaring them.

-- Encrypted secret material (see `crate::keystore::MasterKeyring`).
-- `ciphertext`/`nonce` are AES-256-GCM output; `key_version` names which
-- keyring entry can decrypt this row, so an in-progress key rotation can
-- always decrypt every row (each is re-encrypted under the new version
-- one at a time, not in one all-or-nothing transaction).
CREATE TABLE IF NOT EXISTS secrets (
    key_ref TEXT PRIMARY KEY,
    ciphertext BLOB NOT NULL,
    nonce BLOB NOT NULL,
    key_version INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    rotated_at TEXT
);

-- Provider/profile/MCP-server config, each stored as its own already-
-- validated JSON serialization (the same shape `profiles/list` etc.
-- already return) rather than re-normalized columns -- one place to keep
-- in sync with `crate::provider`/`crate::profile`/`crate::mcp_servers`'s
-- own (de)serialization, matching `custom_agents`' precedent of not
-- duplicating a Rust type's shape into bespoke SQL columns for anything
-- beyond the primary key.
CREATE TABLE IF NOT EXISTS provider_configs (
    name TEXT PRIMARY KEY,
    json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS profile_configs (
    name TEXT PRIMARY KEY,
    json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_server_configs (
    name TEXT PRIMARY KEY,
    json TEXT NOT NULL
);
