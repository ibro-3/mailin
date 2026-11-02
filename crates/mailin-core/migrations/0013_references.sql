-- Add refs column to messages for thread depth computation.
-- Stores the parsed References header (space-separated Message-IDs).

ALTER TABLE messages ADD COLUMN refs TEXT;

-- Index for thread depth computation lookups
CREATE INDEX idx_messages_refs ON messages(refs) WHERE refs IS NOT NULL;