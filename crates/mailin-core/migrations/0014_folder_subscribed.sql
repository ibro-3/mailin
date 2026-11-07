-- Track IMAP folder subscription state for state-aware UI (M3a-4).
ALTER TABLE folders ADD COLUMN subscribed INTEGER NOT NULL DEFAULT 1;
