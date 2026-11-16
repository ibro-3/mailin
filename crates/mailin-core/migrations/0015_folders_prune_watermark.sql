-- Local-cache retention: per-folder watermark of the highest UID already
-- pruned by the retention pass. Sync never re-imports UIDs at or below the
-- watermark, so a full re-scan does not resurrect messages the user's
-- retention setting discarded.
ALTER TABLE folders ADD COLUMN prune_watermark INTEGER NOT NULL DEFAULT 0;
