-- Virtual folders for unified inbox and other cross-account views.
-- These are not stored as actual folders but as saved queries.

CREATE TABLE virtual_folders (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL,  -- 'unified_inbox', 'starred', 'all_mail'
    query_json      TEXT NOT NULL,  -- {account_ids: [...], folder_roles: [...]}
    created_at      INTEGER NOT NULL,
    UNIQUE (account_id, name)
) STRICT;

CREATE INDEX idx_virtual_folders_account ON virtual_folders(account_id);