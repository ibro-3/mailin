-- Message templates stored per-account with {{variable}} placeholders.
-- Templates are local-only (no network sync) and used to prefill compose windows.

CREATE TABLE templates (
    id          INTEGER PRIMARY KEY,
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    subject     TEXT NOT NULL DEFAULT '',
    body        TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (account_id, name)
) STRICT;

CREATE INDEX templates_by_account ON templates(account_id);