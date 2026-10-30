-- Contact autocomplete: auto-seeded from observed mail, plus manual entries.
-- Per-account, indexed for prefix search ordered by frequency/recency.

CREATE TABLE contacts (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email           TEXT NOT NULL,
    display_name    TEXT NOT NULL DEFAULT '',
    last_used_at    INTEGER,
    times_seen      INTEGER NOT NULL DEFAULT 0,
    manual          INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, email)
) STRICT;

CREATE INDEX idx_contacts_email ON contacts(account_id, email);
CREATE INDEX idx_contacts_search ON contacts(account_id, times_seen DESC, last_used_at DESC);