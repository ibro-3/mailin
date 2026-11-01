-- Plugin system: Lua scripts with sandboxed execution.
-- Plugins hook into events and can propose rule actions.

CREATE TABLE plugins (
    name        TEXT PRIMARY KEY,
    version     TEXT NOT NULL,
    source_path TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    manifest_json TEXT NOT NULL DEFAULT '{}',
    permissions_json TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_plugins_enabled ON plugins(enabled);