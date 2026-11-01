-- Rules/Filters: per-account rules evaluated on new incoming mail during sync.
-- Actions reuse existing engine operations.

CREATE TABLE rules (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 1,
    priority        INTEGER NOT NULL DEFAULT 0,
    conditions_json TEXT NOT NULL,  -- [{field, op, value}]
    actions_json    TEXT NOT NULL,  -- [{kind, value}]
    stop            INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    UNIQUE (account_id, name)
) STRICT;

CREATE INDEX idx_rules_account ON rules(account_id, priority DESC, id);