-- Mailin schema, version 1.
--
-- Accounts keep their full settings as JSON so that the configuration file can
-- stay free of credentials and so that server settings travel with the account
-- when it is renamed. Passwords never enter this database.

CREATE TABLE accounts (
    id            TEXT PRIMARY KEY NOT NULL,
    email         TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE folders (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    delimiter       TEXT NOT NULL DEFAULT '/',
    role            TEXT NOT NULL DEFAULT 'other',
    uid_validity    INTEGER,
    uid_next        INTEGER,
    highest_modseq  INTEGER,
    last_synced_at  INTEGER,
    UNIQUE (account_id, name)
) STRICT;

CREATE INDEX folders_by_account ON folders(account_id);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    folder_id       INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid             INTEGER NOT NULL,
    message_id      TEXT,
    thread_key      TEXT,
    subject         TEXT NOT NULL DEFAULT '',
    from_name       TEXT,
    from_addr       TEXT,
    to_addrs        TEXT NOT NULL DEFAULT '',
    cc_addrs        TEXT NOT NULL DEFAULT '',
    sent_at         INTEGER,
    received_at     INTEGER NOT NULL,
    snippet         TEXT,
    size            INTEGER,
    -- Space separated IMAP flags, for example "\Seen \Flagged".
    flags           TEXT NOT NULL DEFAULT '',
    has_attachments INTEGER NOT NULL DEFAULT 0,
    body_state      INTEGER NOT NULL DEFAULT 0,
    UNIQUE (folder_id, uid)
) STRICT;

CREATE INDEX messages_by_folder ON messages(folder_id, received_at DESC, uid DESC);
CREATE INDEX messages_by_thread ON messages(thread_key) WHERE thread_key IS NOT NULL;

CREATE TABLE bodies (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    text_plain TEXT,
    html       TEXT,
    fetched_at INTEGER NOT NULL
) STRICT;

CREATE TABLE attachments (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    part_id    TEXT NOT NULL,
    filename   TEXT,
    mime_type  TEXT NOT NULL DEFAULT 'application/octet-stream',
    size       INTEGER,
    content_id TEXT,
    is_inline  INTEGER NOT NULL DEFAULT 0,
    UNIQUE (message_id, part_id)
) STRICT;

CREATE TABLE outbox (
    id            INTEGER PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    raw           BLOB NOT NULL,
    envelope_json TEXT NOT NULL,
    state         TEXT NOT NULL DEFAULT 'queued',
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX outbox_by_state ON outbox(state, created_at);

-- Full text index over headers and (once fetched) plain text bodies.
-- The rowid mirrors messages.id, which keeps updates and deletes cheap and
-- lets searches join straight back to the message table.
CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject,
    from_name,
    from_addr,
    to_addrs,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
