-- Scheduled actions for snooze, send-later, and future automated actions.
-- Modeled on the outbox pattern: persistent queue with a worker that wakes on due time.

CREATE TABLE scheduled_actions (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    due_at          INTEGER NOT NULL,
    kind            TEXT NOT NULL,          -- 'snooze_message' | 'send_later' (future)
    payload_json    TEXT NOT NULL,
    state           TEXT NOT NULL DEFAULT 'pending',  -- pending | done | failed
    created_at      INTEGER NOT NULL,
    last_error      TEXT
) STRICT;

CREATE INDEX idx_scheduled_due ON scheduled_actions(state, due_at);