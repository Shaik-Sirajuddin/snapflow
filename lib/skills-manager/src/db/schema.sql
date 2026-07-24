-- Fixed schema. Table names/shapes here are never caller-configurable --
-- see README.md#schema in the plan doc (memory/acpx/gen/plans/acpx-skills/).
--
-- vendor_id throughout is a custom-agent-format id (e.g. "codex-acp",
-- "claude-acp"), not a second embedding application -- panel-rust is the
-- sole caller. skill_targets deliberately has no separate agent_id column:
-- vendor_id already carries that meaning.

CREATE TABLE IF NOT EXISTS skills (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    content_hash TEXT NOT NULL,
    central_path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(name, content_hash)
);

CREATE TABLE IF NOT EXISTS skill_owners (
    skill_id TEXT NOT NULL REFERENCES skills(id),
    vendor_id TEXT NOT NULL,
    registered_at INTEGER NOT NULL,
    PRIMARY KEY (skill_id, vendor_id)
);

CREATE TABLE IF NOT EXISTS skill_targets (
    id TEXT PRIMARY KEY,
    skill_id TEXT NOT NULL REFERENCES skills(id),
    vendor_id TEXT NOT NULL,
    target_path TEXT NOT NULL,
    mode TEXT NOT NULL,
    status TEXT NOT NULL,
    last_synced_at INTEGER,
    last_error TEXT,
    UNIQUE(vendor_id, skill_id, target_path)
);
