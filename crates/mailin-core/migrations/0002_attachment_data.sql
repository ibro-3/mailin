-- Attachment bytes are cached together with the body. Fetching the complete
-- message already downloads every part, so storing the decoded contents costs
-- no extra network round trip and makes "Save as…" work offline.
--
-- The column stays nullable: messages cached before this migration carry only
-- metadata until their body is refreshed.

ALTER TABLE attachments ADD COLUMN data BLOB;

-- Content IDs are looked up when an HTML body references `cid:` URLs.
CREATE INDEX attachments_by_content_id
    ON attachments(message_id, content_id)
    WHERE content_id IS NOT NULL;
