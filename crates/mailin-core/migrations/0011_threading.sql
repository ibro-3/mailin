-- Threading/conversation view support.
-- Add thread_root_key (the References-root message-id) and thread_reply_count to messages.

ALTER TABLE messages ADD COLUMN thread_root_key TEXT;
ALTER TABLE messages ADD COLUMN thread_reply_count INTEGER NOT NULL DEFAULT 0;

-- Backfill: thread_root_key = COALESCE(thread_key, message_id) for existing rows
UPDATE messages SET thread_root_key = COALESCE(thread_key, message_id);

-- Index for fast thread root lookups and reply counting
CREATE INDEX idx_messages_thread_root ON messages(thread_root_key) WHERE thread_root_key IS NOT NULL;
CREATE INDEX idx_messages_thread_root_folder ON messages(folder_id, thread_root_key);